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
- Specimen keyspace comparison;
- Specimen findings about sharded maps/counters being absent.

Compromise:

- use normal Rust `HashMap`, `BTreeMap`, counters, and vectors inside isolates;
- Tina owns routing, placement, fanout, timeout, full/closed outcomes, and trace;
- first form is examples plus small helper types where repetition proves value,
  not a polished data-structure library.

API home:

- start with a small `tina_runtime::sharded` module for reusable contracts;
- keep concrete counter/map services example-owned until repetition proves
  they belong in the crate;
- exported first-form types should be boring policy/result shapes, not a
  framework:
  - `ShardPlacement`;
  - `ShardPlacementReport`;
  - `ShardServiceTable`;
  - `ScatterGatherConfig`;
  - `ScatterGatherReport`;
  - partial-outcome enums for full/closed/timeout/failed target;
- do not export a `ShardedMap` or `ShardedCounter` product type in first form
  unless examples prove the generic shape is truly stable.

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
- Key input is canonical bytes, not arbitrary `Hash`.
- Owner services validate placement before mutating shard-owned state.
- Scatter/gather has bounded fanout.
- Aggregates report partial success, full, closed, timeout, and failed shard.
- Hot-key pressure remains visible.
- Retry/backoff is caller policy, never automatic helper fog.
- State stays owned by isolate/shard.
- Docs must say these are local multi-shard patterns, not clustering.

## Rocks

1. **API Home And Surface Boundary**

   Required decision:

   - reusable helper contracts live in `tina_runtime::sharded`;
   - examples own concrete service implementations;
   - docs name this as "local multi-shard patterns", not a distributed
     collection library;
   - public exports are limited to placement/config/report types;
   - `ShardServiceTable` is an explicit address table, not service discovery;
   - any generic service wrapper stays private or example-local unless the
     implementation proves it removes real duplication.

   Done when:

   - module exists or plan explicitly says "example-only first";
   - public/private line is written down before code lands;
   - README/user-guide points users at examples for full service shapes.

2. **Key Placement Helper**

   Requirements:

   - deterministic key-to-shard mapping over an explicit ordered shard list;
   - first-form key input is `&[u8]` / `str` via a tiny canonical-bytes helper,
     not arbitrary `Hash`;
   - hash function/scheme is named and stable for this phase;
   - no assumption that `ShardId`s are contiguous or start at zero;
   - duplicate shard ids rejected at construction;
   - empty shard list rejected at construction;
   - placement version/scheme named in the report;
   - hash input, hash scheme, and shard ordering documented;
   - visible placement report;
   - shard-count-change behavior documented as "mapping may change" unless a
     later stable-ring scheme is introduced;
   - wrong-shard/wrong-key rejection pattern;
   - tests for stable mapping over non-contiguous ids;
   - live and simulator use byte-identical placement for the same shard list.

   Suggested first form:

   ```rust
   let placement = ShardPlacement::new([ShardId::new(10), ShardId::new(30)])?;
   let owner = placement.owner_for_bytes(key.as_bytes());
   let report = placement.report();
   ```

3. **Shard Service Table**

   Requirements:

   - explicit ordered `(ShardId, Address<M, R>)` list;
   - same shard list/order as `ShardPlacement`;
   - duplicate shard ids rejected;
   - missing owner shard returns typed `MissingShard`;
   - stale generation / stopped target surfaces as `Closed`, not refresh magic;
   - no hidden registry thread;
   - no `Arc<Mutex<HashMap<...>>>` side registry in examples;
   - restart refresh is out of scope unless a test explicitly owns it.

   Suggested first form:

   ```rust
   let services = ShardServiceTable::new([
       (ShardId::new(10), counter_10),
       (ShardId::new(30), counter_30),
   ])?;
   let target = services.address_for(owner)?;
   ```

4. **Sharded Counter First Form**

   Requirements:

   - increment local shard counter;
   - read local;
   - aggregate total across shards;
   - partial aggregate reports missing/full/timeout shards;
   - owner isolate validates `ctx.shard_id() == placement.owner_for_bytes(key)`
     before mutating keyed state where a key is involved;
   - wrong owner returns typed `WrongShard { expected, actual }`;
   - simulator tests for reorder/failure.

5. **Sharded Map Pattern**

   Requirements:

   - key routes to owner shard;
   - owner isolate uses normal map;
   - owner isolate re-checks placement before get/put/delete;
   - wrong owner returns typed `WrongShard { expected, actual }`;
   - get/put/delete with timeout;
   - full/closed/timeout visible;
   - no global lock, no shared map.

6. **Scatter/Gather Helper**

   Requirements:

   - bounded fanout;
   - explicit `ScatterGatherConfig` or equivalent;
   - named `max_targets`;
   - named collector mailbox capacity;
   - named per-target in-flight cap or proof that at most one call per target
     is in flight;
   - named result capacity equal to or below `max_targets`;
   - per-target timeout;
   - aggregate timeout;
   - public or example-owned partial result type;
   - cancellation/shutdown behavior visible;
   - no hidden unbounded reply collection;
   - requester-full behavior tested: aggregate reply can be rejected and traced;
   - collector-full behavior tested: helper reports `Full`, not hidden queueing.

   First-form config shape:

   ```rust
   pub struct ScatterGatherConfig {
       pub max_targets: usize,
       pub collector_mailbox_capacity: usize,
       pub per_target_timeout: Duration,
       pub aggregate_timeout: Duration,
   }
   ```

   Zero values are config errors, not silent clamps.

7. **Hot-Key Pressure Policy**

   Requirements:

   - hot owner shard can reject full;
   - retry/backoff pattern through Tina timers is caller-owned and explicit;
   - no automatic retry inside placement/map/scatter helpers;
   - user must opt in and therefore owns idempotency/safety;
   - max attempts is required;
   - backoff timer outcomes are visible in trace;
   - metrics/report includes full/timeout per shard;
   - report separates first-attempt `Full`, retry success, retry exhaustion,
     and timeout;
   - no automatic queue growth.

8. **Examples**

   Add examples:

   - sharded counter;
   - sharded map/keyspace;
   - scatter/gather aggregate;
   - partial failure report.

   Reuse Specimen comparison style where useful.

9. **DST**

   Required histories:

   - reorder fanout replies;
   - one shard full;
   - one target isolate closed;
   - simulator-only failed shard, unless a deterministic live failed-shard seam
     already exists;
   - aggregate timeout;
   - hot key pressure;
   - saved seed for partial aggregate.

   Live failure mechanism:

   - first form uses stopped target isolates or unknown target addresses to
     prove `Closed`/failed-target reporting without crashing worker threads;
   - live whole-shard failure is optional and only allowed through an existing
     deterministic runtime seam;
   - simulator owns failed-shard DST because it can make that failure
     repeatable.

## Required Proof

- Multi-shard sharded counter works live and in sim.
- Sharded map routes keys deterministically live and in sim.
- Owner services reject wrong-shard mutation with typed `WrongShard`.
- Service address table reports missing/stale/closed targets without hidden
  refresh.
- Scatter/gather reports partial outcomes.
- Scatter/gather has explicit capacity tests: max targets, collector full,
  requester full, aggregate timeout.
- Hot-key pressure is visible as full/timeout, not hidden buffering.
- Hot-key retry, when shown, has explicit max attempts and separates first
  full from retry success/exhaustion.
- Placement over non-contiguous `ShardId`s is stable for a fixed ordered shard
  list and canonical key bytes.
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
- `examples/specimen_sharded_keyspace` (3 smoke tests) is a paired
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
file is reserved for Specimen-comparison findings):

- Scatter/gather coordinators need a `ReplyBridge`-style isolate to
  translate replies between typed addresses (the runtime does not let
  one isolate's `Address<X>` accept a reply typed for `Address<Y>`).
  Every fanout user pattern hits this. Likely a 059 ergonomics
  candidate (e.g., a typed `Address::map_into` or a shipped
  `ReplyAdapter<From, To>` isolate).
