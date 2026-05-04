# 028 Review

## Plan Review 1

Verdict: ready to hand to implementation.

The plan now has the right phase identity: Ranger is not a service-demo phase,
not a Tokio bridge phase, and not a small polish pass. It is the core runtime
substrate completion phase. The size is intentionally allowed to be as large as
needed, because closing early would only move unfinished core questions into
Gemini, Apollo, or service-framework work.

What looks strong:

- The close criterion is clear: after Ranger, later phases should build on Tina
  core instead of reopening runtime/substrate fundamentals.
- The default substrate direction is pinned: continue hardening Betelgeuse
  unless it blocks a required core semantic. Tokio/Monoio/Glommio/Compio remain
  future adapters unless a pause gate deliberately changes that.
- Full-duplex TCP is named as a core question, with an expected lane model:
  listener accept, stream read, stream write, and close rejection while a lane is
  active.
- Cancellation and shutdown are treated as core semantics, not driver cleanup:
  stopped requester, explicit close, runtime shutdown, late completion,
  requester-mailbox-full completion, and timeout races all require direct proof.
- Live Betelgeuse, simulated Betelgeuse, and `tina-sim` parity is load-bearing.
  Divergence must be fixed or recorded as concrete non-overlap.
- Cost work is scoped correctly: allocation/operation counts for named hot
  paths, not wall-clock benchmark theater.
- The phase requires a core/non-core boundary so later service/docs/adapter
  phases know what they can depend on.

Implementation guardrails:

- Start with the capability audit. Do not begin by rewriting TCP or adding an
  adapter.
- Keep service-shaped work minimal. It exists only to catch lifecycle/substrate
  bugs that focused tests miss.
- Keep `review.md` as the progress ledger: capability gaps, design decisions,
  cost evidence, substrate decision, core boundary, and remaining non-claims.
- Use pause gates aggressively if Betelgeuse cannot support a required semantic
  or if a new core primitive appears necessary.

No blocking plan findings remain.

## Implementation Review 1 - Capability Audit

Current live substrate baseline:

- `ThreadedRuntime` already runs a shard-owned `Runtime` on a worker thread.
- `RuntimeDriver` owns runtime time and TCP calls: timer, bind, accept, read,
  write, close, completion harvest, shutdown cancel, and per-call cancel.
- The current Betelgeuse driver has no hidden runtime executor. Progress is
  explicit through `Runtime::step` or worker turns.
- Pending operation admission is bounded by user mailboxes and the driver's
  tracked pending set; no unbounded driver command queue is added in this
  slice.
- Stopped requesters already cancel pending driver calls at the runtime layer,
  and canceled driver calls no longer keep `has_in_flight_calls()` true.

Current gaps:

- TCP pending ownership is still resource-wide for streams. A pending read
  blocks a pending write on the same stream, so full-duplex TCP is not yet a
  Tina substrate capability.
- Per-call TCP cancel is still too broad in the live driver: canceling one
  pending stream operation closes the whole stream resource. That can
  invalidate an unrelated live read/write on the same stream once full-duplex
  is allowed.
- The deterministic simulator models the same stream-wide `ResourceBusy` rule,
  so live/sim parity exists today but the shared rule is too conservative for
  Ranger.
- `CallError::ResourceBusy` still describes a whole-resource conflict, but
  Ranger wants lane conflicts: listener accept lane, stream read lane, stream
  write lane, and close conflicting with any active lane.

Implementation direction for chunk 1:

- Keep Betelgeuse as the live substrate.
- Replace stream-wide pending identity with TCP lanes in both live driver and
  `tina-sim`.
- Allow one pending read and one pending write on the same stream.
- Keep same-lane duplicates rejected as `ResourceBusy`.
- Keep explicit stream/listener close rejected as `ResourceBusy` while any
  relevant lane is pending.
- Make per-call cancel tombstone the selected operation without closing the
  underlying TCP resource. Late completion is swallowed if it arrives; unrelated
  lanes continue normally.

## Implementation Review 2 - TCP Lanes

Chunk 1 landed.

What changed:

- `RuntimeDriver` now states the driver capability contract directly in
  `tina-runtime/src/driver.rs`.
- Live Betelgeuse TCP pending ownership moved from stream-wide resource
  identity to lanes: listener accept, stream read, stream write.
- `tina-sim` now models the same lane rule.
- Same-stream read/write can overlap.
- Duplicate work on the same lane still rejects as `ResourceBusy`.
- Explicit listener/stream close still rejects as `ResourceBusy` while relevant
  lanes are active.
- Per-call cancel no longer closes the underlying TCP resource. It tombstones
  the selected pending operation so requester completion is not delivered and
  quiescence is not blocked.
- A canceled tombstone still reserves its same TCP lane until the substrate
  completion drains. That avoids arming two reads or two writes on one stream
  when the first operation still exists below Tina.

Important semantic note:

Cancellation is now honest about substrate side effects. Once a write has been
submitted to the substrate, Tina guarantees no requester completion after
requester stop, but it does not claim that already-submitted bytes can always be
unsent. This is the right production-shaped contract unless a future substrate
adds true per-operation cancel.

Direct proof added/updated:

- live runtime: read and write can overlap on one stream;
- live runtime: duplicate read on one stream fails `ResourceBusy`;
- live runtime: canceling a pending read does not close the stream before a
  later write;
- live runtime: a canceled pending read keeps the read lane busy until the
  tombstoned substrate completion drains;
- live runtime: canceled delayed write delivers no requester completion after
  late substrate maturity;
- `tina-sim`: read and write on one stream use separate lanes;
- `tina-sim`: duplicate delayed read remains `ResourceBusy`;
- existing close-while-pending read/write proofs stayed green.

Focused test results:

- `cargo +nightly test -p tina-runtime --test call_dispatch` passed.
- `cargo +nightly test -p tina-sim --test io_simulation` passed.

## Implementation Review 3 - Live Worker Shutdown Proof

Added a threaded Betelgeuse-runtime proof for outstanding TCP accept shutdown.

What it proves:

- The real worker loop can own a simulated Betelgeuse I/O loop.
- A worker-thread `TcpAccept` can remain pending without manual runtime
  stepping.
- `BetelgeuseRuntime::shutdown()` cancels the pending TCP call, does not
  deliver a translated message to the stopped requester, and records
  `CallCompletionRejected { call_kind: TcpAccept, reason: RequesterClosed }`.

Focused test result:

- `cargo +nightly test -p tina-runtime --test betelgeuse_substrate` passed.

## Implementation Review 4 - Cost Pressure

Added exact allocation probes for explicit-runtime TCP read and write hot paths
over Betelgeuse simulated I/O.

Measured hot paths after warm-up:

- timer call path: 10 allocations, 1 reallocation;
- TCP read completion path: 13 allocations, 1 reallocation;
- TCP write completion path: 13 allocations, 1 reallocation;
- isolate call path: 9 allocations, 1 reallocation;
- cross-shard send path: 15 allocations, 2 reallocations;
- live Betelgeuse caller-thread ingress handoff: 1 allocation, 0 reallocations.

This confirms the current honest claim: Tina has bounded queues and explicit
completion ownership, but the broader runtime path is not allocation-free.
The TCP counts include current implementation overhead from boxed completion
slots, erased call/translator plumbing, trace/event growth, and user message
allocation in the tested write shape.

Focused test result:

- `cargo +nightly test -p tina-runtime --test multishard_allocation` passed.

## Implementation Review 5 - Minimal Service Smoke

Ranger did not add a broad service suite. Existing smoke is already the right
minimal pressure:

- live explicit-runtime TCP echo over kernel sockets;
- live threaded Betelgeuse TCP echo;
- threaded Betelgeuse over simulated I/O with delayed completions and partial
  writes;
- `tina-sim` multi-shard TCP workload under seeded completion faults.

These workloads exercise bind, accept, read, write, partial write retry,
stream close, listener close, spawn/restartable child setup, worker-thread
progress, and simulator replay without turning Ranger into an examples phase.

Focused test results:

- `cargo +nightly test -p tina-runtime --test tcp_echo` passed.
- `cargo +nightly test -p tina-sim --test multishard_dispatcher` passed.

## Implementation Review 6 - Substrate Decision And Core Boundary

Substrate decision:

- Continue with vendored Betelgeuse as Tina's near-term live substrate.
- Do not build a Tokio/Monoio/Glommio/Compio adapter before Gemini.
- Treat future adapters as implementations of the now-documented
  `RuntimeDriver` contract, not as redesigns of Tina core.
- Revisit substrate choice only if Betelgeuse blocks a core driver semantic,
  not because ecosystem integration would be convenient.

Core after Ranger:

- isolate scheduling and bounded mailboxes;
- local and cross-shard delivery, including bounded cross-shard queues;
- supervision/restart semantics;
- runtime-owned time;
- runtime-owned TCP bind/accept/read/write/close;
- TCP lane ownership: listener accept, stream read, stream write;
- close rejection while relevant TCP lanes are active;
- requester-stop and shutdown cancellation as completion-delivery cancellation,
  not guaranteed undo of already-submitted substrate side effects;
- live Betelgeuse worker runtime and multi-shard worker runtime;
- deterministic `tina-sim` oracle and Betelgeuse simulated-I/O parity for the
  modeled TCP/time semantics;
- narrow allocation/operation evidence for named hot paths.

Non-core after Ranger:

- Tower/Axum/Hyper integration;
- arbitrary futures inside isolate handlers;
- broad protocol libraries;
- a Tokio bridge;
- a stronger per-operation OS cancel guarantee;
- production readiness claims;
- broad throughput benchmarking;
- release docs and packaging.

Remaining honest non-claims:

- TCP write cancellation does not guarantee that already-submitted bytes are
  unsent. Tina guarantees requester completion cancellation, trace visibility,
  and no unrelated lane invalidation.
- Runtime TCP hot paths allocate today. The SPSC mailbox hot path has the
  stronger no-allocation claim; broader runtime paths do not.
- Live multi-shard cross-shard isolate-call reply transport is still not
  claimed; cross-shard isolate calls reject with typed outcome.

Full verification:

- `make verify` passed.

## Implementation Review 7 - Shutdown/Oracle Bug Fixes

External review found two real Ranger bugs.

Fixed live shutdown:

- `BetelgeuseTcp::cancel_pending()` no longer drops completion boxes while a
  Betelgeuse backend may still hold raw completion pointers.
- Shutdown now marks pending TCP ops canceled, closes all TCP resources, drains
  canceled completions for bounded steps, and intentionally leaks any
  still-owned slots rather than risking a dangling completion pointer.
- The threaded simulated-I/O shutdown proof now steps the external
  `SimulatedIO` after runtime shutdown, so the test pressures the exact
  post-shutdown ownership edge.

Fixed simulator oracle stop semantics:

- `tina-sim` now cancels requester-owned backend calls when an isolate stops.
- The cancel path removes timers, pending accepts, pending TCP completions, the
  in-flight call entry, and the stored translator, then records
  `CallCompletionRejected { reason: RequesterClosed }`.
- Timer, accept, read, and write stopped-requester tests now prove immediate
  quiescence after stop rather than waiting for future completion.

This keeps live runtime and oracle aligned: stopped requester cancels
runtime-driver work immediately. Isolate-call waits remain reply/timeout-driven
in this slice and are named in the plan as production-ish follow-up decision
surface.

Focused and full verification:

- `cargo +nightly test -p tina-runtime --test betelgeuse_substrate` passed.
- `cargo +nightly test -p tina-sim --test timer_semantics` passed.
- `cargo +nightly test -p tina-sim --test io_simulation` passed.
- `make verify` passed.

## Implementation Review 8 - Tina/Odin Alignment

Sources checked:

- Peter Mbanugo's "Why Queues Don't Fix Overload";
- `pmbanugo/tina` README;
- Tina concepts docs for isolates, thread-per-core, I/O/data flow,
  backpressure, and supervision.

Spirit alignment is strong:

- Tina-rs keeps the main mental model: Isolates are synchronous state machines
  that return Effects; user code does not become async/await.
- Mailboxes and cross-shard queues are bounded and surface explicit failure.
- Runtime-owned calls make time and TCP scheduler effects, not user syscalls.
- Deterministic simulation is first-class: same isolate-shaped code can be run
  under an oracle with replay and fault injection.
- Shards are stable ownership domains; live Betelgeuse workers keep each shard
  runtime owned by one OS thread.
- Generational addresses, stale-address rejection, supervision restart budgets,
  and abandoned-message tracing match the original safety direction.

Letter differences remain and should stay honest:

- Original Odin Tina is built around preallocated arenas, fixed envelopes, and no
  dynamic allocation after boot. Tina-rs currently allocates in broader runtime
  paths and has measured those costs rather than eliminating them.
- Original Tina's `ctx_send` returns a synchronous `Send_Result` in the handler.
  Tina-rs has immediate ingress outcomes and observed-send outcomes, but ordinary
  Effects still express work for the runtime to interpret on later turns.
- Original Tina uses platform reactors (`io_uring`/`kqueue`/`IOCP`) with
  reactor-owned buffer pools. Tina-rs currently uses a Betelgeuse-backed driver
  contract and boxed completion slots; the substrate shape is honest but not yet
  the same implementation letter.
- Original Tina claims trap-boundary recovery for panics and segfaults. Tina-rs
  catches Rust panics and uses supervision, but it does not structurally recover
  from arbitrary native memory faults inside the same process.
- Original Tina describes one outstanding call/I/O per Isolate, with full-duplex
  protocols decomposed into cooperating Isolates. Ranger's TCP lane model allows
  read/write overlap on one stream at the resource level while preserving one
  returned Effect per handler turn.

Production-ish upgrades suggested by the comparison:

- reduce hot-path allocation pressure or make the allocation budget part of the
  public contract;
- decide whether Rust should grow arena/envelope-style message storage closer to
  Odin Tina, or keep `Box<dyn Any>` as an experimental implementation trade;
- keep tightening true per-operation cancel/resource ownership so canceled
  tombstones do not become long-lived resource friction;
- define the real reactor/substrate adapter story against the settled
  `RuntimeDriver` contract;
- keep the README language careful: Tina-rs is spiritually Tina, but not yet
  Odin Tina's zero-allocation, platform-reactor, segfault-trap implementation.
