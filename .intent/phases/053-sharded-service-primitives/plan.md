# Phase 053: Sharded Service Primitives

## Goal

Give users small Seastar-shaped data/service patterns.

Not distributed data structures. Not magic.

053 answers:

> Can Tina make common shard-owned service state easy without hiding placement,
> pressure, or partial failure?

Near-grug:

> key goes to shard. shard owns map. fanout asks shards. aggregate tells truth.

## Baseline

Already exists:

- shard ids;
- multi-shard runtime;
- cross-shard sends and calls;
- topology reports;
- simulator multi-shard coverage;
- Eiffel keyspace comparison;
- Eiffel findings about sharded maps/counters being absent.

Compromise:

- use normal Rust `HashMap`, `BTreeMap`, counters, and vectors inside isolates;
- Tina owns routing, placement, fanout, timeout, full/closed outcomes, and trace;
- first form is examples plus small helper types where repetition proves value,
  not a polished data-structure library.

## Non-Goals

- No durable distributed database.
- No consensus.
- No automatic rebalancing.
- No transparent shared map.
- No exactly-once update.
- No remoting/clustering requirement.
- No hiding partial failure.
- No stable placement across shard-count changes unless a specific scheme is
  introduced and tested.

## Rules

- Placement must be visible.
- Key-to-shard function must be deterministic.
- Scatter/gather has bounded fanout.
- Aggregates report partial success, full, closed, timeout, and failed shard.
- Hot-key pressure remains visible.
- State stays owned by isolate/shard.
- Docs must say these are local multi-shard patterns, not clustering.

## Rocks

1. **Key Placement Helper**

   Requirements:

   - deterministic key-to-shard mapping;
   - visible placement report;
   - shard-count-change behavior documented;
   - wrong-shard/wrong-key rejection pattern;
   - tests for stable mapping.

2. **Sharded Counter First Form**

   Requirements:

   - increment local shard counter;
   - read local;
   - aggregate total across shards;
   - partial aggregate reports missing/full/timeout shards;
   - simulator tests for reorder/failure.

3. **Sharded Map Pattern**

   Requirements:

   - key routes to owner shard;
   - owner isolate uses normal map;
   - get/put/delete with timeout;
   - full/closed/timeout visible;
   - no global lock, no shared map.

4. **Scatter/Gather Helper**

   Requirements:

   - bounded fanout;
   - per-target timeout;
   - aggregate timeout;
   - public or example-owned partial result type;
   - cancellation/shutdown behavior visible;
   - no hidden unbounded reply collection.

5. **Hot-Key Pressure Policy**

   Requirements:

   - hot owner shard can reject full;
   - retry/backoff pattern through Tina timers;
   - metrics/report includes full/timeout per shard;
   - no automatic queue growth.

6. **Examples**

   Add examples:

   - sharded counter;
   - sharded map/keyspace;
   - scatter/gather aggregate;
   - partial failure report.

   Reuse Eiffel comparison style where useful.

7. **DST**

   Required histories:

   - reorder fanout replies;
   - one shard full;
   - one shard closed/failed;
   - aggregate timeout;
   - hot key pressure;
   - saved seed for partial aggregate.

## Required Proof

- Multi-shard sharded counter works live and in sim.
- Sharded map routes keys deterministically live and in sim.
- Scatter/gather reports partial outcomes.
- Hot-key pressure is visible as full/timeout, not hidden buffering.
- Docs say these are primitives/patterns, not a database.
- Docs say this is not remoting or clustering.

## Done Means

- Users have copyable shard-owned data patterns.
- Tina looks more Seastar-shaped in the app layer.
- Placement, pressure, and partial failure stay visible.

## First-form landing notes

`tina_runtime::sharded` ships the small contract surface for phase 053:

- `ShardPlacement` / `ShardPlacementReport` / `ShardHashScheme` — explicit
  ordered shard list, FNV-1a 64 bytes-mod-count placement, `owner_for_bytes`
  / `owner_for_str`, name + scheme + version visible on the report. Name
  is a runtime-derived `String` (not `&'static str`). Empty or duplicate
  shard lists are typed `ShardPlacementError` and dedup uses `BTreeSet`.
- `ShardServiceTable<M, R>` — explicit ordered `(ShardId, Address<M, R>)`
  table that must match the placement shard list verbatim. Lookup is
  O(log n) via an internal `BTreeMap<ShardId, usize>` index built at
  construction; no `Arc<Mutex<...>>`, no hidden registry thread. Lookup
  misses surface `MissingShard`. `address_for_bytes` / `address_for_str`
  route through placement and cannot fail given the structural invariant.
- `WrongShard { expected, actual }` — owners must re-check
  `placement.owner_for_bytes(key) == ctx.shard_id()` before mutating keyed
  state and return this typed error on mismatch. The runtime cannot prove
  ownership; only the handler can.
- `ScatterGatherConfig` — explicit `max_targets`, collector mailbox
  capacity (>= max_targets), per-target timeout, aggregate timeout.
  Validated; result vector capacity is bounded by `max_targets`.
- `ScatterGatherReport<T>` + `ScatterGatherTargetOutcome<T>` —
  partial-aggregate report that names `Replied` / `Full` / `Closed` /
  `Timeout` / `AggregateTimeout` / `MissingShard` per target. No hidden
  unbounded reply collection.
- `HotKeyAttemptReport` + `HotKeyAttemptOutcome` — caller-owned retry
  shape. Distinguishes first-attempt accept, first-attempt full, in-loop
  retry full (`full_retry_total` is recorded explicitly so retries do not
  silently look like single attempts), retry success, retry exhaustion,
  timeout, closed.
- All four error types (`ShardPlacementError`, `ShardServiceTableError`,
  `MissingShard`, `WrongShard`, `ScatterGatherConfigError`) implement
  `Display + std::error::Error` so they bubble through `?` into
  `Box<dyn Error>`.

Proof landed:

- `tina-runtime/src/sharded.rs` unit tests (22): placement empty /
  duplicate rejects, non-contiguous shard distribution (asserts every
  shard reachable), referential transparency, report fields, runtime-
  derived placement names, service-table mismatch, missing-shard,
  scatter/gather config validation, partial-aggregate counts, hot-key
  intermediate-and-terminal recording, and `Display + Error` on every
  error enum.
- `tina-runtime/tests/sharded_primitives.rs` (11) drives the explicit-step
  `MultiShardRuntime` for: sharded counter first form, owner re-check
  returning `WrongShard`, sharded map first form (put/get/del with
  WrongShard on writes), service-table `MissingShard` lookup, bounded
  scatter/gather happy-path, scatter/gather **partial-failure** with
  `MissingShard` (target outside table) and **`Full`** (cap-0 target via
  `send_observed`), hot-key caller-owned retry loop with strict
  bookkeeping (Full really observed; retries actually succeed), and
  hot-key retry exhaustion with budget=0. Includes a frozen FNV-1a
  byte-identical-with-sim placement check.
- `tina-runtime/tests/sharded_threaded.rs` (2) drives a real
  `ThreadedMultiShardRuntime` (Betelgeuse worker threads) over a sharded
  counter and a `WrongShard` re-check, so the live cross-shard path is
  proved — not just the deterministic in-process explicit-step runtime.
- `tina-sim/tests/sharded_dst.rs` (3) proves byte-identical placement
  (frozen mapping shared with the runtime test), reorder-invariance
  under `LocalSendFaultMode::DelayByRounds { one_in: 1, rounds: 1 }`
  with an explicit `assert_ne!` on the trace records (so the test fails
  if the seeded perturbation does **not** actually move events), and
  that `MultiShardReplayArtifact` carries the simulator config for
  partial-aggregate seed recovery.

Out-of-scope for first form:

- DST histories for `Closed` / `Timeout` / `AggregateTimeout` per-target:
  the report shape is wired and the partial-failure live tests cover
  `Full` and `MissingShard`. Per-target close + virtual-time aggregate
  timeout require a richer simulator coord scaffold; left for follow-on.
- Generic `ShardedMap` / `ShardedCounter` product type: not introduced
  until more shipped examples prove the shape is stable.

Surfaced finding:

- Scatter/gather coordinators need a `ReplyBridge`-style isolate to
  translate replies between typed addresses (the runtime does not let
  one isolate's `Address<X>` accept a reply typed for `Address<Y>`).
  Every fanout user pattern hits this. Recorded in
  `examples/FINDINGS.md` as a future ergonomics item — likely a 059
  candidate.
