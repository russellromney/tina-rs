# 031 Ruud Lubbers Review

## Plan Review 1

Verdict: structurally on-shape and ready to start implementation.

Good grug:

- The phase starts with measurement, not optimization vibes.
- It is explicitly allowed to optimize broadly, which matches the human
  direction. It does not artificially limit itself to one obvious hot path.
- Cross-thread preparation is named as a first-class reason for the work:
  caller-thread ingress, worker commands, cross-shard transport, erasure,
  completions, and trace pressure all get inspected before more live substrate
  behavior lands.
- Semantic guardrails are sharp: no weakening trace/replay, bounded queues,
  stale-address rejection, shutdown/cancel visibility, or runtime-owned I/O to
  make counts prettier.
- The proof bar requires before/after counts and direct tests for changed
  costs.

Main implementation risks:

1. Allocation tests can become brittle if they assert broad counts that include
   unrelated trace/test setup. Keep probes narrow and warm up first.
2. Optimizing boxed erasure too early could create unsafe or complicated code.
   Prefer obvious clone/temp/preallocation wins first, then only introduce pools
   or arenas if measurements say they matter.
3. Wall-clock benchmark numbers should not become correctness gates. Use them
   as notes only if needed.
4. Performance work can accidentally hide semantics by disabling trace or
   replay. Review must treat that as a bug, not an optimization.

Implementation should begin with an audit of
`tina-runtime/tests/multishard_allocation.rs`, `tina-mailbox-spsc` allocation
tests, the live runtime command path, driver pending-operation storage,
cross-shard transport, and the new Willem Drees local-production workload.

## Implementation Audit 1

Current measured cost evidence:

| Path | Current evidence | Current count / shape |
|---|---|---|
| SPSC mailbox send/recv | `tina-mailbox-spsc/tests/allocation_accounting.rs` | 0 allocations / 0 reallocations after warm-up |
| SPSC full/closed error | `tina-mailbox-spsc/tests/allocation_accounting.rs` | 0 allocations / 0 reallocations |
| single-shard local send | `tina-runtime/tests/multishard_allocation.rs` | operation rounds only: `[1, 1, 0]` |
| multi-shard send | `tina-runtime/tests/multishard_allocation.rs` | 15 allocations / 2 reallocations |
| live Betelgeuse ingress handoff | `tina-runtime/tests/multishard_allocation.rs` | caller thread: 1 allocation / 0 reallocations |
| isolate call reply path | `tina-runtime/tests/multishard_allocation.rs` | 9 allocations / 1 reallocation |
| timer call path | `tina-runtime/tests/multishard_allocation.rs` | 10 allocations / 1 reallocation |
| TCP read completion path | `tina-runtime/tests/multishard_allocation.rs` | 13 allocations / 1 reallocation |
| TCP write completion path | `tina-runtime/tests/multishard_allocation.rs` | 13 allocations / 1 reallocation |
| local production workload | no operation count yet | semantic/backpressure proof only |
| spawn/restart | no allocation count yet | semantic proof only |
| trace pressure | no allocation count yet | semantic proof only |
| batch effect | no allocation count yet | semantic proof only |

Likely cost sources:

- **Semantic costs**
  - trace events and causal ids;
  - runtime-owned call bookkeeping;
  - bounded command queues;
  - generation/stale-address checks;
  - request/reply timeout bookkeeping;
  - owned user payload movement.
- **Implementation costs likely removable**
  - trace `Vec` growth without reserve in predictable test/workload paths;
  - `Effect::Batch(Vec)` allocation for common two-effect batches;
  - per-step temporary `Vec` construction in call harvesting;
  - cloning trace snapshots for live control queries;
  - worker command boxing for every live ingress command;
  - repeated `Vec<u8>` clones in proof workloads that can move ownership.
- **Implementation costs risky to change**
  - erased `Box<dyn Any>` message storage in the generic runtime;
  - boxed call translators;
  - boxed spawn/restart adapters;
  - completion-slot boxes around Betelgeuse.
  These are central to the current type-erased runtime shape. Optimize only
  with direct measurement and focused regression proof.
- **Debug/proof costs**
  - replay artifacts;
  - full trace retention;
  - trace snapshots returned from live worker handles;
  - checker failure capture.
- **User payload costs**
  - `Vec<u8>` request/response payloads in TCP workloads;
  - scripted peer input/output buffers.

Immediate gaps to fill before optimization:

- spawn allocation count;
- restart allocation count;
- trace growth allocation count under repeated no-op sends;
- `Effect::Batch` allocation count for two-effect batch;
- local-production workload operation-count probe;
- cross-thread command allocation beyond the current one-message ingress
  handoff.

## Implementation Slice 1

Verdict: first broad runtime allocation pass landed.

Changes:

- Added missing allocation probes for:
  - two-send `Effect::Batch`;
  - spawn;
  - direct restart;
  - repeated trace/event pressure.
- Preallocated runtime-owned bookkeeping vectors at runtime construction:
  entries, child records, supervisors, trace, in-flight calls, translators,
  pending isolate calls, round-message scratch, and driver-completion scratch.
- Changed runtime-created mailboxes to store erased message boxes directly.
  This removes the old box -> downcast into typed mailbox -> box again on
  receive cycle for runtime-created mailboxes. User-provided typed mailboxes
  passed to `Runtime::register` keep the typed adapter path.
- Reused a runtime-owned round-message scratch buffer instead of allocating a
  fresh `Vec<Option<DeliveredMessage>>` on every `step()`.
- Changed driver completion harvesting to append into runtime-owned scratch
  storage instead of returning a fresh completion vector every step.
- Changed timer and isolate-call timeout harvesting to scan/remove in place
  instead of building `due` and `still_pending` vectors.
- Changed stopped-requester driver-call cancellation and TCP shutdown drain to
  remove in place instead of take-and-rebuild.
- Removed temporary restart index vectors from direct restart and supervised
  panic restart paths.
- Removed an avoidable live multi-shard trace key snapshot allocation.

Before/after measured counts:

| Path | Before | After |
|---|---:|---:|
| multi-shard send hot path | 15 alloc / 2 realloc | 3 alloc / 0 realloc |
| isolate call reply path | 9 alloc / 1 realloc | 2 alloc / 0 realloc |
| timer call path | 10 alloc / 1 realloc | 2 alloc / 0 realloc |
| TCP read completion path | 13 alloc / 1 realloc | 6 alloc / 0 realloc |
| TCP write completion path | 13 alloc / 1 realloc | 6 alloc / 0 realloc |
| two-send batch path | unmeasured | 4 alloc / 0 realloc |
| spawn path | unmeasured | 6 alloc / 0 realloc |
| direct restart path | unmeasured | 4 alloc / 0 realloc |
| repeated trace pressure, 16 sends | unmeasured at phase start; first probe saw 48 alloc / 1 realloc | 16 alloc / 0 realloc |
| live Betelgeuse caller-thread ingress | 1 alloc / 0 realloc | 1 alloc / 0 realloc |

What the remaining allocations mostly mean:

- one box is still paid when external/user messages enter erased runtime
  storage;
- send/call/reply paths still box payloads and translators where the current
  erased runtime model needs type erasure;
- spawn/restart still box adapters/factories and allocate runtime-owned child
  storage;
- TCP read/write still pay user-payload and driver/completion costs;
- trace retention remains enabled and is not weakened for speed.

Cross-thread preparation:

- Caller-thread ingress remains at one allocation.
- Runtime worker turns no longer allocate round snapshots or driver completion
  vectors on every step.
- Runtime-created mailboxes no longer re-box every message on receive.
- Live cross-shard send still allocates because payload erasure/transport is
  still boxed, but the coordinator no longer clones shard metadata per
  destination turn.

Focused verification:

- `cargo +nightly test -p tina-runtime --test multishard_allocation --test local_production_runtime --test call_dispatch --test betelgeuse_substrate --test tcp_echo --test consumer_api`
  passed.

## Implementation Slice 2

Verdict: simulator/oracle allocation shape now matches the runtime hardening
direction.

Changes:

- Preallocated simulator-owned entries, child records, supervisors, trace,
  timers, TCP resources, pending backend work, in-flight calls, translators,
  isolate calls, and round-message scratch storage.
- Reused simulator round-message scratch storage instead of allocating one
  `Vec<Option<DeliveredMessage>>` per simulator step.
- Changed simulator timer and isolate-call timeout harvesting to scan/remove in
  deterministic order instead of allocating `due` and `still_pending` vectors.
- Removed temporary restart index vectors from simulator direct restart and
  supervised panic restart paths.
- Changed simulator stopped-requester backend-call cancellation to remove
  in-place instead of take-and-rebuild.
- Added a local-production oracle operation-count probe for the composed
  server-shaped workload:
  - parent boot deliveries: 2;
  - server deliveries: 41;
  - final event count: 220;
  - TCP write completions under partial writes: 16;
  - isolate-call failures under bounded worker pressure/timeout: 2.

Live test correction:

- `live_local_server_routes_tcp_through_bounded_worker_pressure` no longer
  asserts exactly one native-thread `TargetFull` event. Native client ordering
  can legitimately create more than one full outcome. The test still proves the
  user-visible response set and requires bounded pressure to be visible in the
  trace.

Focused verification:

- `cargo fmt --all && cargo test -p tina-sim` passed.
- `cargo test -p tina-sim --test local_production_runtime` passed after the
  operation-count probe was added.
- `cargo +nightly test -p tina-runtime --test multishard_allocation --test local_production_runtime --test call_dispatch --test betelgeuse_substrate --test tcp_echo --test consumer_api`
  passed.
- `make verify` passed.

## Implementation Review 1

Verdict: no blocking findings.

What I checked:

- Allocation wins do not remove trace events, replay artifacts, failure
  outcomes, bounded queues, stale-address rejection, shutdown rejection, or
  runtime-owned I/O.
- Runtime-created mailbox erasure is documented on `MailboxFactory`; user-owned
  typed mailboxes passed through `Runtime::register` still use the typed
  adapter path.
- Runtime and simulator both keep deterministic due-time ordering while
  replacing `due`/`still_pending` vectors with in-place ordered removal.
- Restart loops do not change child-record cardinality while iterating; restart
  replaces the existing child slot rather than appending a new child record.
- Live native TCP test now asserts the production guarantee instead of exact OS
  scheduling: full pressure must be visible, but the exact number of full
  isolate-call failures may vary.

Remaining honest costs:

- External ingress, sends, calls, replies, live worker commands, and
  cross-shard transport still pay boxed-erasure costs.
- `Effect::Batch(Vec<_>)` still allocates at the user surface and then erases
  into a second vector internally.
- Betelgeuse completion slots still allocate per pending TCP op.
- These are now named costs, not hidden costs.

## Implementation Slice 3

Verdict: second pass found one high-value cross-thread-prep cleanup.

Changes:

- `MultiShardRuntime` now stores sorted `shard_ids` once at construction
  instead of rebuilding them every `step()`.
- `MultiShardRuntime::step()` no longer clones `shard_indexes` once per
  destination shard. It borrows disjoint coordinator fields directly.
- `MultiShardSimulator` got the same stored-shard-id and no-index-clone
  treatment so the oracle and live explicit-step runtime keep the same shape.

Measured effect:

- multi-shard send hot path: `9 alloc / 0 realloc` after slice 1 -> `3 alloc /
  0 realloc` after slice 3.

Focused verification:

- `cargo +nightly test -p tina-runtime --test multishard_allocation` passed.
- `cargo test -p tina-sim --test multishard_dispatcher` passed.
- `make verify` passed.

## Implementation Review 2

Verdict: one scratch-reserve bug found and fixed.

Bug:

- Runtime and simulator reused `round_messages` scratch storage, but used
  `reserve(entries.len() - capacity)` after `clear()`. In Rust, `reserve()`
  takes additional capacity relative to current length, not desired target
  capacity. The old math could fail to reserve enough before push-time growth
  when isolate count exceeded the initial scratch capacity.

Fix:

- Added `reserve_round_message_scratch(...)` in both runtime and simulator.
  It now explicitly ensures scratch capacity covers the current entry count
  before collection starts.

Regression proof:

- Runtime unit test directly proves scratch reserve covers more than the
  initial eight-entry capacity.
- Simulator unit test proves the same reserve helper shape.
- Runtime allocation integration test registers 12 isolates and proves the
  warmed idle step is `0 alloc / 0 realloc`.

Focused verification:

- `cargo +nightly test -p tina-runtime --test multishard_allocation` passed.
- `cargo test -p tina-runtime round_message_scratch_reserve_covers_more_than_initial_capacity` passed.
- `cargo test -p tina-sim round_message_scratch_reserve_covers_more_than_initial_capacity` passed.
- `make verify` passed.

## Close

Verdict: 031 is closed.

What changed the project:

- Tina now has a current narrow numerical cost model instead of allocation
  vibes.
- The worst easy framework-owned costs found in this phase were removed across
  runtime, simulator, driver, and multi-shard coordinator code.
- The hot SPSC mailbox no-allocation claim remains protected.
- Runtime/simulator allocation claims remain deliberately narrow; boxed
  erasure, call translators, trace storage, replay records, completion slots,
  and user payloads can still allocate.
- Medium cost rocks are explicitly carried in `ROADMAP.md`, not silently
  forgotten.

Closeout updates:

- `ROADMAP.md` moved Ruud Lubbers to delivered and refreshed the runtime
  allocation-story row.
- `CHANGELOG.md` records the phase changes.
- `.intent/SYSTEM.md` now records the durable allocation/performance rules.
- `make verify` passed.

## Implementation Slice 4

Verdict: easy worth-it-now cleanup landed without weakening Galileo staging.

Changes:

- `MultiShardRuntime` and `MultiShardSimulator` now prebuild an indexed
  source-shard/destination-shard queue table instead of using a `BTreeMap`
  queue store on the hot path.
- Cross-shard queues are now persistent double buffers. This removes the old
  per-step map take/restore allocation pressure while preserving 020's
  next-global-step visibility rule for newly emitted remote sends.
- `BetelgeuseDriver` now gives timer, TCP resource, and pending-operation
  vectors small initial reserves.
- Medium rocks were added to `ROADMAP.md` instead of squeezed into the easy
  slice: batch small path, worker-command boxing, sizing knobs, trace retention
  policy, typed fast paths, and completion-slot pooling/slabbing.

Measured effect:

- multi-shard send hot path: `3 alloc / 0 realloc` after slice 3 -> `1 alloc /
  0 realloc` after slice 4.
- multi-shard delivery progress stays `[1, 1, 0]`, proving the allocation win
  did not turn remote sends into same-step visibility.

Design note:

- A naive `Effect::Batch2` is not an easy win because `Effect` is recursive.
  Inline storage would need careful enum layout work; boxing the child effects
  would likely trade one allocation shape for another. It is roadmapped as a
  medium design rock.

Focused verification:

- `cargo +nightly test -p tina-runtime --test multishard_allocation` passed.
- `cargo test -p tina-sim --test multishard_dispatcher` passed.
