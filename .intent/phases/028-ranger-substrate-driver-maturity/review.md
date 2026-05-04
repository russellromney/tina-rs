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
