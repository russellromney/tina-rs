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
- **`require_owner_bytes` / `require_owner_str`** — owner re-check
  helpers on `ShardPlacement` that fold the canonical
  `if owner != ctx.shard_id() { return Err(WrongShard { ... }) }`
  pattern into one call. Returns `Result<ShardId, WrongShard>`.
- `ShardServiceTable<M, R>` — explicit ordered `(ShardId, Address<M, R>)`
  table that must match the placement shard list verbatim. Lookup is
  O(log n) via an internal `BTreeMap<ShardId, usize>` index built at
  construction; no `Arc<Mutex<...>>`, no hidden registry thread. Lookup
  misses surface `MissingShard`. `address_for_bytes` / `address_for_str`
  route through placement and cannot fail given the structural invariant.
- **`ShardServiceTable::from_placement` / `try_from_placement`** — table
  builders that take a placement and a registration closure. Removes
  the manual `for shard in placement.shards() { entries.push(...) }`
  loop. `try_from_placement` returns `ServiceTableBuildError<E>` so
  fallible runtimes (e.g. `ThreadedMultiShardRuntime`) compose cleanly.
- `WrongShard { expected, actual }` — owners must re-check the key
  before mutating keyed state and return this typed error on mismatch.
  The runtime cannot prove ownership; only the handler can.
- `ScatterGatherConfig` — explicit `max_targets`, collector mailbox
  capacity (>= max_targets), per-target timeout, aggregate timeout.
  Validated; result vector capacity is bounded by `max_targets`.
- `ScatterGatherReport<T>` + `ScatterGatherTargetOutcome<T>` —
  partial-aggregate report that names `Replied` / `Full` / `Closed` /
  `Timeout` / `AggregateTimeout` / `MissingShard` per target. **Public
  ordering contract: `outcomes` preserves caller-supplied target
  order**, regardless of reply arrival order, so per-target results are
  index-addressable and log output stays deterministic.
- **`ReplyAdapter<M, T, S>`** — generic isolate that translates `M` into
  `T` (via `impl From<M> for T`) and forwards to a coordinator
  `Address<T>`. Replaces the hand-written `ReplyBridge` isolate every
  scatter/gather coordinator used to need. `Call = RuntimeCall<M>` so
  the same primitive registers in the explicit-step `MultiShardRuntime`,
  `ThreadedMultiShardRuntime`, and the simulator.
- `HotKeyAttemptReport` + `HotKeyAttemptOutcome` — caller-owned retry
  shape. Distinguishes first-attempt accept, first-attempt full, in-loop
  retry full (`full_retry_total` is recorded explicitly so retries do not
  silently look like single attempts), retry success, retry exhaustion,
  timeout, closed.
- All five error types (`ShardPlacementError`, `ShardServiceTableError`,
  `ServiceTableBuildError<E>`, `MissingShard`, `WrongShard`,
  `ScatterGatherConfigError`) implement `Display + std::error::Error` so
  they bubble through `?` into `Box<dyn Error>`.

Proof landed:

- `tina-runtime/src/sharded.rs` unit tests (29): placement empty /
  duplicate rejects, non-contiguous shard distribution (asserts every
  shard reachable), frozen FNV-1a v1 mapping (cross-crate anchor),
  report fields, runtime-derived placement names, **`require_owner_*`
  ok/wrong cases**, service-table mismatch, missing-shard,
  **`from_placement`/`try_from_placement` builder happy + error
  paths**, scatter/gather config validation, partial-aggregate counts,
  hot-key intermediate-and-terminal recording, `Display + Error` on
  every error enum, and two property tests (random shard list + keys:
  owner-in-set + determinism, full reachability with enough keys).
- `tina-runtime/tests/sharded_primitives.rs` (14) drives the explicit-step
  `MultiShardRuntime` for: sharded counter first form, owner re-check
  returning `WrongShard`, sharded map first form (put/get/del with
  WrongShard on writes **and on reads** — wrong-shard `Get` returns
  `Err(WrongShard)`, not `Ok(None)`), service-table `MissingShard`
  lookup, bounded scatter/gather happy-path, scatter/gather
  **partial-failure** with `MissingShard` (target outside table),
  **`Full`** (cap-0 target via `send_observed`), and **`Closed`**
  (target isolate stopped before fanout — restart/regeneration story),
  hot-key caller-owned retry loop with strict bookkeeping (Full really
  observed; retries actually succeed), hot-key cap-0 retries that
  exercise `full_retry_total` against a real runtime, hot-key retry
  exhaustion with budget=0, **`scatter_gather_report_preserves_caller_supplied_target_order`**
  (targets supplied as `[91, 3, 17]` come back in that exact order),
  and **`scatter_gather_report_ordering_holds_under_mixed_partial_outcomes`**
  (mixed `MissingShard`/`Full`/`Replied` in non-sorted target order
  still preserves the contract). Both `ScatterCoord`s in the file now
  use the shipped `ReplyAdapter` primitive instead of hand-written
  bridge isolates. Includes a frozen FNV-1a byte-identical-with-sim
  placement check.
- `tina-runtime/tests/sharded_threaded.rs` (2) drives a real
  `ThreadedMultiShardRuntime` (Betelgeuse worker threads) over a sharded
  counter and a `WrongShard` re-check, so the live cross-shard path is
  proved — not just the deterministic in-process explicit-step runtime.
- `tina-sim/tests/sharded_dst.rs` (4) proves byte-identical placement
  (frozen mapping shared with the runtime test), reorder-invariance
  under `LocalSendFaultMode::DelayByRounds { one_in: 1, rounds: 1 }`
  with an explicit `assert_ne!` on the trace records (so the test
  fails if the seeded perturbation does **not** actually move events),
  that `MultiShardReplayArtifact` carries the simulator config for
  partial-aggregate seed recovery, **virtual-time `AggregateTimeout`**
  — a `QuietCounter` absorbs `Get` without replying, the coord
  schedules `sleep_then(aggregate_timeout)`, virtual time advances
  past the deadline, and the report records `AggregateTimeout` for the
  silent target — and **`scatter_gather_report_in_sim_preserves_caller_supplied_target_order`**
  (sim-side ordering contract test, so a future sim-coord refactor
  can't quietly drop the contract). The aggregate-timeout coord uses
  the shipped `ReplyAdapter` to translate counter replies into its
  own message type.
- `examples/eiffel_sharded_keyspace` (3 smoke tests) is a paired
  Tokio-vs-Tina sharded keyspace. Same SET/GET/DEL/SUM/QUIT script,
  same FNV placement, byte-identical `Report`. Tokio side is
  `Vec<Arc<Mutex<HashMap>>>` with hand-rolled placement; Tina side is
  `ShardPlacement` + `ShardServiceTable` + per-shard `Store` isolates
  with owner re-check and a `Driver` that walks the script via
  `call(...).reply(...)` continuations.

Out-of-scope for first form:

- DST histories for `Closed` / per-target `Timeout`: `Closed` has live
  coverage via the explicit-step restart test; per-target `Timeout`
  (distinct from the aggregate timer) needs a richer coord that
  watches each target deadline separately, left for follow-on.
- Generic `ShardedMap` / `ShardedCounter` product type: not introduced
  until more shipped examples prove the shape is stable.

Surfaced finding (recorded here, not in `examples/FINDINGS.md` — that
file is reserved for Eiffel-comparison findings):

- Scatter/gather coordinators need a `ReplyBridge`-style isolate to
  translate replies between typed addresses (the runtime does not let
  one isolate's `Address<X>` accept a reply typed for `Address<Y>`).
  Every fanout user pattern hits this. Likely a 059 ergonomics
  candidate (e.g., a typed `Address::map_into` or a shipped
  `ReplyAdapter<From, To>` isolate).
