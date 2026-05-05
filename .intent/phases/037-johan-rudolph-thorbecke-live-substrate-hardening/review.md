# Review

## Implementation Review 1

Verdict: first Thorbecke slice was the right shape, but three sharp stones
needed fixing before building more substrate work on it.

Findings fixed:

1. Live multi-shard ignored `storage_lane_capacity`.
   `BetelgeuseBackedMultiShardRuntime::with_config` validated only command
   capacity, then built each shard with the default explicit-step driver. The
   public `LocalApp::multi_shard(...).storage_lane_capacity(...)` knob therefore
   did not control live shard storage admission. Fixed by validating storage
   capacity and constructing each live shard with
   `BetelgeuseDriver::with_io_loop_and_storage_capacity`.

2. Direct explicit-step runtime picked up a hidden storage worker.
   `Runtime::new` and `Runtime::with_betelgeuse_io_loop` are the semantic
   oracle paths. Routing them through the bounded worker lane added background
   thread timing where the oracle should stay direct and step-shaped. Fixed by
   making storage inline for direct explicit-step drivers and keeping the
   bounded storage worker lane on live `BetelgeuseBackedRuntime` /
   `BetelgeuseBackedMultiShardRuntime`.

3. `CallError::Canceled` was public surface without a user-receivable path.
   Stopped-requester cancellation is surfaced as rejected completion/trace, not
   as a delivered call failure to a live requester. Fixed by removing the
   premature public error variant and leaving the first live storage error
   vocabulary to `StorageFull` and `StorageClosed`.

Proof added:

- `driver::tests::explicit_driver_storage_completes_inline_without_pending_lane`
  pins direct driver storage as inline/no hidden pending lane.
- `betelgeuse_multishard_rejects_zero_storage_lane_capacity` pins live
  multi-shard config validation.
- Existing storage-lane proofs still cover full admission, late completion
  swallowing, and canceled queued work.

No open P1/P2 findings after these fixes. Remaining Thorbecke work is not bug
fixing; it is the next slice: broader live pressure/accounting and larger
composed app proofs.

## Next Slice Start

Added `local_app_tcp_service_journals_before_replying_to_client` as the first
larger composed live proof: one app service binds TCP, accepts a real client,
reads bytes, appends the journal before replying, closes runtime-owned TCP
resources, shuts down through `LocalApp`, then replays the journal from disk.

This is still not the whole Thorbecke closeout. It proves the shape stacks for
TCP plus persistence, but broader overload/accounting and pressure numbers are
still next rocks.

Added `LocalAppTerminalReport::summary()` as the first terminal-accounting
piece. It is deliberately derived from the final trace, so grug has one truth:
if a terminal report says calls completed, failed, were rejected, messages were
abandoned, or persistence recovered, those counts come from trace events the
user can inspect.

Added a zero-allocation pressure test for terminal summary scans. The summary
can be used during shutdown/reporting without becoming a new hidden hot-path
cost.

Found and fixed a stronger storage-full semantics issue: channel capacity alone
made live `StorageFull` depend on whether the worker had already dequeued a
job. Capacity now counts total accepted pending work, so running work still
applies pressure. Added a user-shaped `LocalApp` proof that one journal append
is accepted, the next returns `StorageFull`, trace records the failure, and
replay sees only the accepted record.

Added the full live service proof Thorbecke wanted:
`local_app_cross_shard_tcp_request_persists_before_client_reply`. It runs two
live shard workers, accepts a real TCP client on one shard, sends the payload
to a persistence worker on another shard, appends the journal there, sends an
ack back across the shard boundary, replies to the client only after
persistence, shuts down, and replays the journal from disk.

## Implementation Review 2

Testing gap found: Thorbecke had strong live e2e tests, but the DST harness was
still mostly proving smaller rocks. That left the simulator weaker than the
live path for the exact service shape users care about.

Fixed by adding `multishard_tcp_persistence_service_replays_under_seeded_dst_faults`.
The test runs scripted TCP ingress on shard A, cross-shard journal append on
shard B, ack back to shard A, and peer-visible TCP output only after durable
append. It runs under seeded local-send and TCP-completion perturbation, asserts
bytewise replay artifact equality on a second run, checks the durable journal
image replays to the expected record, and uses a checker that would fail if
`TcpWrite` completes before `JournalAppended`.

This is the better proof shape: live e2e says the OS-backed service works, and
sim DST says the same semantic flow is replayable under perturbation.

User asked whether DST can exercise the flow harder. Added
`multishard_tcp_persistence_service_handles_overlap_partial_io_and_seed_sweep`.
It runs three overlapping scripted clients, cuts reads into small chunks,
forces TCP writes to complete partially, routes each chunk across shards to a
journal-owning isolate, assigns monotonic journal indexes there, and reruns the
same workload for several seeds with exact artifact equality. The proof asserts
peer-visible output, replayable journal records, monotonic indexes, cross-shard
request/ack counts, and that partial writes really happened.

Added `dst_randomized.rs` to cover quiet corners that named scenarios do not
shake enough. It generates deterministic operation histories from fixed seeds.
The single-shard history mixes delayed local sends, delayed timers, stop,
panic, mailbox pressure, and stale sends. The multi-shard history mixes bounded
remote queues, bursts that hit `Full`, stopped workers, unknown remote targets,
and replies back to the coordinator. Both rerun each generated history and
require exact artifact equality, visible send outcomes, causal trace links, and
no handler turn after a shard-local isolate has stopped.

The first multi-shard run caught a bug in the test invariant itself: it tracked
stopped isolates by `IsolateId` only, but isolate ids are shard-local. Fixed the
checker to track `(ShardId, IsolateId)`, which is a useful reminder that DST
checks must respect Tina's shared-nothing identity model.

## Extra DST Pressure Pass

User asked to try all obvious DST opportunities. Added more rocks:

- Persistence fault matrix: generated append/snapshot/recover/bad-index
  histories replay exactly and produce durable images that recover cleanly.
- Supervision plus persistence: a child persists, panics, restarts under
  supervision, and recovers from journal.
- TCP cancellation matrix: seeded pending accept/read/write cancellation proves
  tombstones reject late completions and leave no in-flight work behind.
- Live-vs-sim differential: explicit runtime, simulator, and Betelgeuse-backed
  runner now agree on send/stop/closed-rejection semantics for the same
  workload.
- Bridge ingress model DST: bounded ingress plus timeout cancellation is
  modeled so cancelled queued work cannot mutate service state.
- Shrinker smoke proof: generated DST failures can be minimized by deleting
  irrelevant operations while preserving the replayable failure.

Targeted suites passed, then full `make verify` passed after closeout edits:
format, check, workspace tests, doctests, loom SPSC, docs, and clippy with
`-D warnings`.

Full verify also exposed a live TCP worker-pressure test that assumed one
specific OS scheduling interleaving (`alpha`, `worker-full`, `worker-timeout`).
The runtime guarantee is smaller and better: accepted TCP clients must receive
a bounded, explicit outcome, and at least one must surface worker pressure.
Updated the test to assert that semantic contract instead of a fragile native
thread ordering.

## Closeout

Thorbecke now closes on its own terms.

What landed:

- bounded live storage lane for file/persistence work;
- named `StorageFull` and `StorageClosed` outcomes;
- storage cancellation and shutdown behavior with late completion swallowing;
- trace-derived `LocalAppTerminalReport::summary()`;
- full live multi-shard/thread-per-core TCP plus persistence service proof;
- user-shaped storage overload and recovery proofs;
- randomized DST pressure for single-shard, multi-shard, persistence,
  supervision, TCP cancellation, bridge ingress, shrinking, and live-vs-sim
  parity.

No open P1/P2 findings remain in this review. The remaining production rocks
are future phases, not Thorbecke halfwork: first-class DST harness extraction,
live topology/failure-domain hardening, and deeper I/O/storage maturity.
