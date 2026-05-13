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
| Single-shard runtime delivery | `tina-runtime` has deterministic trace IDs and causal links, registration-order stepping, local send dispatch, local spawn dispatch, typed ingress, stop-and-abandon, panic capture, address generations, runtime-owned parent-child lineage, restartable child records, direct-child `RestartChildren` execution, supervised panic restart with policy/budget config, an assertion-backed task-dispatcher proof package, and generated-history property tests. | Supervision is still narrow: panic-triggered only, runtime-lifetime budget only, and no timed budget windows. The generated-history model is bounded and does not prove arbitrary user programs. |
| Failure isolation | Unwinding handler panics become runtime events; the panicking isolate stops and the same round continues deterministically. | This is not Tina-Odin's OS trap boundary. Rust segfault isolation, shard quarantine, and `panic = "abort"` behavior are out of scope unless a later phase explicitly designs them. |
| Multi-shard runtime/sim | `tina-runtime` and `tina-sim` expose multi-shard explicit-step runners with root placement, global event/call ids, bounded shard-pair queues, next-step-only remote visibility, deterministic harvest order, source-time versus destination-time delivery stages, simulator replay, user-shaped dispatcher proofs, sealed address-local remote-failure behavior, and shard-local supervision/restart ownership. The live Betelgeuse multi-shard runner has bounded ingress, bounded cross-shard transport, live cross-shard isolate-call request/reply transport, first-class live topology reports, visible queue-pressure counters, per-shard `Running`/`Stopped`/`Failed` lifecycle reports, partial trace snapshots after shard failure, terminal shutdown reports with topology/resource/error truth retained together, advisory shard/core ownership reporting, and a bounded remote-inbound drain budget. | Hard OS thread pinning, peer quarantine, shard-restart propagation, and cross-shard child ownership remain future work. |
| Replayability | Runtime traces are deterministic across repeated identical single-shard runs, including generated operation histories and small generated dispatcher workloads. Trace replay proofs can reconstruct worker completions and restart outcomes from the runtime event model alone. `tina-sim` adds virtual time, replay records, seeded delays/reordering over timer-wake/local-send/TCP-completion behavior, checker failures, spawn/supervision replay, scripted TCP simulation, multi-shard replay under default and non-default seeded configs, and multi-shard checker failure replay. | Real substrate liveness faults remain future work; current explicit-step shard-liveness non-claims are sealed. |
| Runtime allocation story | The SPSC mailbox hot path is tested for no per-message allocation after warm-up. Ruud Lubbers pins a narrow numerical runtime cost model for selected hot paths: multi-shard send, isolate call, timer, TCP read/write, batch, spawn/restart, trace pressure, live ingress, and high-cardinality idle stepping. Runtime and simulator now reuse per-step scratch and prebuild coordinator storage where tests prove the warmed path. `PreallocationConfig` lets live systems reserve runtime-owned metadata at setup. | No broad runtime/simulator allocation-free claim is supported yet; boxed erasure, traces, replay records, backend-owned completion slots, call translators, and user payloads may still allocate. |
| Reference examples | A Rust task-dispatcher proof package and a TCP echo proof package both exist with matching runnable examples, backed by assertions rather than logs alone. The echo proof now keeps the listener alive across a one-client smoke run, a sequential multi-client run, and a bounded-overlap run, then closes the listener cleanly and exits. | These are still proof workloads, not a broad production-server claim or benchmark story. |
| Runtime-owned I/O | `tina` names a runtime-owned call effect family (`Effect::Call(I::Call)` plus `Isolate::Call`) and an ordered batch effect (`Effect::Batch(Vec<Effect<I>>)`) for closed-set sequencing of existing effects. `tina-runtime` executes time, TCP server/client operations, local file/path operations, local persistence, UDP, bounded DNS, native TLS client and server lanes, bounded process runs, runtime shutdown notification, and Unix `SIGINT`/`SIGTERM` capture through Tina-owned driver rails with cancellation, shutdown, trace, and same-resource lane ownership. `tina-sim` scripts TCP, file/path, persistence, UDP, DNS, TLS, process, and signal rails for deterministic replay/DST. Capability reports name lane-backed, poll-backed, completion-backed, tombstoned, drained, and unsupported shapes. | Live-substrate liveness faults, remoting, clustering, native database clients, and production-grade streaming remain future work. |
| Local persistence | `tina-runtime` exposes local snapshot/journal helpers with explicit append-before-apply semantics, snapshot `last_journal_index`, journal `record_index`, visible truncated/corrupt/commit-uncertain recovery outcomes, persistence trace events, and bounded live storage-lane admission for snapshot/journal work. `tina-sim` captures `DurableImage` path-to-bytes state for replay. | This is not a database, durable mailbox, durable work queue, or exactly-once system. Directory fsync and rename-commit support remain platform/backend scoped. Already-started local filesystem work cannot be preempted; a full nonblocking storage reactor remains future work. |
| Native service protocols | `tina-http` now gives Tina a native HTTP/1.1 stack: parser/framing, connection/listener isolates, request/response types, routing helpers, bounded limits, visible overload, graceful close paths, native client, native HTTPS, response streaming, response-side and request-side chunked transfer, chunked client decode, keepalive client pool, server-side keepalive, and parser/DST coverage. | HTTP/2, gRPC, WebSocket, richer web-framework ergonomics, and full listener/connection simulator replay remain future work. |
| Ecosystem bridges | `tina-tokio-bridge`, `tina-rpc-tokio`, `tina-tower-bridge`, `tina-reqwest-bridge`, `tina-sqlite-bridge`, `tina-sqlx-bridge`, and `tina-aws-bridge` exist as bounded bridge shapes. The docs name runtime cost, weakened replay boundary, explicit caps, shutdown truth, caller-owned retry, typed DB/S3 outcomes, and bridge tracing. | SQS/Dynamo/SNS, smol, bridge convention audit, common bridge setup extraction, and bridge crate/folder layout remain future work. |
| App/service ergonomics | Specimen work has turned repeated example pain into typed result waiters, bounded observation handles, reply aliases, TCP loop helpers, pressure summaries, HTTP router sugar, sharded placement/table helpers, deferred replies, `CallContext` reply obligation, `RequestContext` multi-turn replies, typed child refs via `spawn_observed`, cancellation handles, deadlines, pending-call sets, bounded pools, host-burst helpers, single-call gates, reqwest/DB classifiers, capacity reports, and host `call_blocking` scripts where appropriate. | `reply_with_current_request(call, ...)` helper polish, cross-isolate paired registration, generic scatter/gather happy-path helpers, bridge setup unification, host-side scenario/test ergonomics, weighted/shared capacity, race/join helpers, and more real-world specimens remain future work. |

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
- Blue Whale: advisory shard/core ownership reporting, runtime-owned metadata
  preallocation knobs, bounded remote-inbound drain budget, fake-substrate
  contract proof, cooperative fairness proof, checked Seastar-principles table,
  and combined e2e coverage tying core ownership, preallocation, remote budget,
  and cross-shard calls together.
- Portable local runtime completion: canonical public-path
  `LocalMultiShardSystem` service harness, runtime-call continuation replies
  through I/O/persistence, executable budget manifest, visible placement and
  backpressure policy proofs, service-level DST with saved seed and shrink,
  portable cost-smoke command, and focused CI gate.
- Baobab: executable local-service readiness matrix, Baobab user-service
  gauntlet over TCP/timer/DNS/process/file/persistence/cross-shard call/
  shutdown, live multi-shard sibling-survives-failed-shard proof, selected
  LocalSystem rail/backpressure e2e gate, saved-seed service/persistence/bridge
  DST histories, real Tina local timing smoke rows, all folded into
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

## Near-term roadmap

These phases are about finishing Tina as a local bounded, shared-nothing
framework before public release-story work.

| Phase | Purpose |
|---|---|
| **081 bridge convention audit** | We now have enough bridges (`tokio`, `tower`, `reqwest`, `sqlite`, `sqlx`, `rpc-tokio`) to audit install/config/closer/metrics/tracing/late-result naming. Output should be boring: conventions, docs, maybe tiny shared helpers. Do not build a bridge framework unless three repeated shapes demand the same code. |
| **082 capacity modeling round 2** | Finish the capacity story beyond count-only reports: user-defined weight units, explicit `UnboundedForNow` with expiry, shard-local shared capacity scopes, DST/CI capacity assertions, and migration of body/pool/bridge reports into the shared reporting shape. Weight is not "memory"; it is a user-defined cost unit that is easy to inspect when wrong. |
| **070 sharded data and placement structures** | Continue 053's owned-data direction into reusable shapes: sharded counters, sharded maps, session tables, batch `group_by_owner`, stale generation detection, owner-side validation helpers, hot-key reports, and fanout read helpers. The goal is not shared mutable data; the goal is owned per-shard data with easy routing and explicit wrong-shard/closed/full outcomes. |
| **083 production service layers** | Package the copied service patterns into docs and small APIs: HTTP routing + state + DB pool + outbound keepalive + graceful shutdown + tracing + capacity assertions. Not a framework on top of Tina; a "real service skeleton" proving an LLM can assemble the layers safely. |
| **085 race / join helpers** | Tina-shaped equivalents for the useful parts of `select!`, `join!`, and `JoinSet`: bounded named call groups, first-success race with visible loser cancellation, join-all with partial deadline report, `CallContext`/`RequestContext` integration, and no hidden retry or anonymous branch outcomes. This builds on 086 call contexts, 072 pending call sets, 079 cancellation, and ideally 084 child refs. |
| **RequestContext helper polish** | Tiny helper after 086, no full phase unless it grows: add `reply_with_current_request(call, f)` on call/observed-call builders as sugar for `call.into_request_context()` plus `reply_with_request(...)`. The helper consumes explicit call authority, not generic `Context`. Docs should still show the expanded form once so users know where caller authority lives. |
| **087 WebSocket first form** | Native WebSocket over Tina-owned HTTP/TCP/TLS rails. Use `tungstenite` core for frame parsing if useful, but Tina owns session isolates, ping/pong, close handshake, slow-reader pressure, inbound/outbound caps, cancellation, trace, and specimens. First form is server rooms plus a tiny test client if needed; not a broad web framework or full client crate. Plan: `.intent/phases/087-websocket-first-form/plan.md`. |
| **088 AWS bridge first form** | In review in PR #73. Bounded S3 bridge first: object put/get/head/delete, explicit body caps, caller-owned retry/idempotency, close/drain, timeout/late-result truth, metrics, and fake-local CI. SQS/Dynamo/SNS wait until S3 proves the bridge shape. Plan: `.intent/phases/088-aws-bridge-first-form/plan.md`. |
| **089 live trace to sim replay workflow** | Turn DST into an ops workflow: user-guided live capture of typed ops/config/topology/pressure facts, projection into `tina-sim`, compare traces, shrink the bad case. No magic "logs become replay." Missing facts must be explicit. Plan: `.intent/phases/089-live-trace-to-sim-replay-workflow/plan.md`. |
| **090 resource lifecycle unification** | Audit runtime rails and bridge resources so open/start, ready, use, cancel, close, drain, terminal report, and pressure report mean the same boring thing everywhere. Fix small mismatches; split big semantic changes into their own PRs. |
| **056 native HTTP/2 service stack** | Second-form HTTP after HTTP/1 body/chunked/cancellation semantics are boring. Adopt sync codec crates where they exist; Tina owns sockets, flow control, stream state, bounded queues, DST, and trace. Not a full RFC/tonic clone. |
| **057 native gRPC service stack** | Third-form RPC after 056 HTTP/2 lands. Small layer on top: `prost`, generated Tina-shaped service trait template, typed status, unary and server-streaming first form, client and server, DST for status/cancel. Bidirectional streaming and interceptor feature-parity are later. |
| **Alpaca rename** | Before public launch, rename the project/crates/docs away from Tina to Alpaca so the lineage is respectful and clear: independently maintained Rust framework, inspired by Peter Mbanugo's Tina/Odin and Seastar, not an official Tina port. |
| **Barend Biesheuvel visible flow ergonomics** | Optional high-level ergonomics only after the local runtime core feels boring: a `flow!`-style authoring surface that preserves named suspension points, visible failure policy, trace step names, and ordinary Tina message/effect expansion. No fake async, no hidden retries, no hidden queues. |

## Capability layers still needed

These are not planning phases. They are capability gaps to close as real
implementation slices pull on them. Each entry should land as boring code,
tests, and specimens when the adjacent work proves the shape.

| Capability | Build | User outcome |
|---|---|---|
| Child lifecycle and join | Typed child-start observation, typed child address/result, join-many result collection, parent-stop child cleanup, and replacement-address refresh without trace spelunking. | A service can spawn workers/sessions, wait for them, stop them, and survive restarts without inventing a side registry. |
| Stronger owned-work cancellation | Uniform cancel/close/drain rules for Tina-owned timer/TCP/TLS/process/file/DNS rails, with tombstoned late completions and prompt resource release where the backend can support it. | `cancel` stops waiting everywhere, and for Tina-owned work it also releases the owned resource as strongly as the rail allows. |
| Capacity scopes | User-defined weight units, explicit unbounded-with-expiry escape hatch, shard-local shared capacity scopes, and capacity assertions usable in DST/CI. | Users tune from evidence instead of vibes, and related surfaces can share one visible budget. |
| Live trace to sim replay | Capture live inputs/resource completions/topology facts into a replayable history, project them into `tina-sim`, and report exact divergence. | A production bug can become a small deterministic "bug in a box" instead of a log-reading séance. |
| Race/join helpers | Honest `race`/`join`/`join set` helpers over messages/effects that preserve named suspension points, cancellation, capacity, and trace outcomes. | Common `select!` / `join!` Tokio shapes can be written in Tina without hiding which branch won or what got cancelled. |
| Timer vocabulary | Replay-safe interval, backoff, jitter, debounce, throttle, and deadline propagation helpers built on runtime time, not ambient clocks. | Periodic and retrying services stop hand-rolling sleep loops and still replay deterministically. |
| Production service skeleton | A small executable skeleton tying HTTP/HTTPS, routing, DB pool, outbound keepalive, graceful shutdown, tracing, capacity assertions, and DST seed capture together. | An LLM can copy one real-service shape and replace a moderate Tokio app without stitching ten specimens by hand. |
| Protocol and client breadth | HTTP/2, gRPC, WebSocket, AWS bridge, and broader RPC hardening, using sync codecs where possible while Tina owns I/O/backpressure. | Common Tokio protocol workloads have native Tina paths instead of falling back to Tokio bridges at the first real boundary. |
| Compile-time diagnostics | `#[diagnostic::on_unimplemented]` and macro error polish for `Isolate`, message/send/call bounds, non-`Send`, non-`'static`, wrong shard, and wrong reply shapes. | Humans and LLMs see "this message is not Send" instead of trait soup. |
| Resource lifecycle unification | One boring vocabulary across runtime resources and bridges: open/start, ready, use, close, cancel, drain, terminal report, pressure report. | No stream/file/process/bridge worker can be stranded invisibly; every resource has the same mental model. |
| Health and readiness | Runtime/service-level readiness and liveness surfaces, distinct from process alive, with typed reasons and optional HTTP/RPC exposure. | Deployments can stop sending traffic before shutdown and can report "not ready because DB pool closed" honestly. |
| Shutdown orchestration graph | Ordered shutdown helpers: stop ingress, cancel/close pools, drain in-flight work, flush batchers, close bridges/resources, emit final report. | Graceful shutdown becomes a copied Tina program instead of bespoke stop-message choreography. |
| Backpressure policies | Small explicit policy objects for shed, bounded wait, retry with backoff, degrade, and close, all returning typed outcomes. | Services choose pressure behavior at call sites without losing `Full`/`Closed`/`Timeout` truth. |
| Runtime-owned recurring work | Local cron/periodic task patterns for compaction, health checks, token refresh, session expiry, and metric flush, with missed-tick policy. | Long-lived services get boring recurring work that is bounded and replayable. |
| Config and budget manifest | A structured service config manifest for mailbox caps, pool caps, body caps, deadlines, retry budgets, and capacity policies; printable and diffable. | Operators and coding agents can see all knobs before the service runs and can include them in replay cases. |
| Service topology report | A service-shaped topology report naming isolates, shards, addresses, mailboxes, pools, bridges, resources, capacities, and lifecycle states. | Users can ask "what is running?" and get a useful answer without scraping raw traces. |
| Bounded event/log sink | Runtime-owned or specimen-proven bounded log/metric/event sinks with visible overflow/drop policy. | Observability does not become the first hidden unbounded queue in an otherwise bounded Tina service. |
| State snapshot and restore | Blessed snapshot/journal/restore patterns for shard-owned state, including append-before-apply proofs and torn-write recovery specimens. | Ordinary services can restart with state safely, not only demos with in-memory maps. |
| Saga / compensation pattern | Typed multi-step workflow pattern over DB, HTTP, pools, and services, with explicit compensation, timeout, cancellation, and partial-failure reports. | Multi-resource business workflows become readable Tina state machines instead of ad hoc async control flow. |
| Load and soak harness | A reusable harness for long runs that records capacity high-water, full counts, latency-ish summaries, resource leaks, and trace fingerprints. | Teams can prove a Tina service stays bounded for an hour before claiming it is production-shaped. |

Closed follow-ups from Phase 074 (HTTP body streaming, native HTTP/1):

The 074 slice shipped server-side body streaming with pressure metrics,
`IterBodySource`, chunked transfer-encoding emit, and a specimen. The
follow-ups that were intentionally deferred from that PR have now mostly
landed in their owning phases.

| Group | Lands in | Items |
|---|---|---|
| Cancel surface | **079 cancellation round 2** | Landed: connection-to-source cancel signal on wire failure (`ResponseChunkMsg::Cancel`) for known-length and chunked streaming responses. |
| Capacity report | **082 capacity modeling round 2** | Still active: migrate body/bridge/pool reports into the shared `CapacitySurfaceReport` shape where useful. Periodic / live-tick metric emit and shared scopes belong here, not in one HTTP slice. |
| Chunked symmetric | **080 HTTP body chunked symmetric** | Landed: client-side chunked decoding and server-side chunked request bodies through the streaming pull model. |

Do not resurrect the old "chunked is deferred" wording. The remaining body
work is HTTP/2/WebSocket/protocol breadth and broader capacity-report polish,
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
| Live trace ↔ sim trace identity | Record a trace under `ThreadedRuntime`, replay against `Simulator` with the same seed and inputs | The direct end-to-end DST claim: that *production* and *simulation* produce byte-identical traces, not just sim-to-sim. Currently only sim-to-sim is asserted. |
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

## Later capability roadmap

These are real Tina directions, but they should not be treated as launch
blockers for the first local-runtime story.

| Phase | Purpose |
|---|---|
| **054 userspace TCP research door** | Name the future kernel-bypass/userspace TCP contract and the measurements that would justify it. This is deliberately not an implementation phase: no DPDK, no packet parser, no NIC driver, no launch promise. Kernel TCP plus Betelgeuse's Linux `io_uring` backend stays the real path until evidence says otherwise. |
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
