# Phase 082: Capacity Modeling Round 2

## Status

- Ready to implement.
- Builds on Phase 071 PR 1: count reports, tuning mode, policy,
  `CapacitySurfaceReport`, `CapacitySummary`, and count assertions.
- Not blocked by 085 race/join or RequestContext helper polish.

## Grug Truth

Count is not memory.

Weight is what the user says a thing costs.

Shared scope is one shard-local budget used by more than one surface.

Unbounded is allowed only when loud, named, and expiring.

Every `Full` must say what filled.

## Goal

Make capacity useful for real services, not just count queues.

Ship:

- user-defined weight units;
- one real weighted surface;
- shard-local shared capacity scope;
- explicit `UnboundedForNow` with expiry;
- ugly no-expiry escape hatch, rejected by safe policies;
- capacity assertions usable in tests/DST;
- docs/specimen showing `unknown -> measured -> fixed`.

Keep it boring. Do not build a global memory manager.

## Non-Goals

- exact heap measurement;
- recursive payload sizing;
- process-wide/global budget;
- Prometheus/exporter work;
- converting every surface in the repo;
- hiding `Full` with retry.

## Rock 0: Re-Read Current Shape

Start by reading:

- `tina/src/capacity.rs`;
- `tina-runtime/src/capacity.rs`;
- `tina-runtime/src/pool.rs`;
- `tina-runtime/src/deferred.rs`;
- HTTP body metrics/cap code;
- bridge pressure reports for sqlite/sqlx/reqwest if nearby.

Write a tiny status note at the top of this file before coding:

- what surfaces already report count;
- which surface will get weight;
- which two surfaces will share one scope.

## Rock 1: Weight Vocabulary

Add a small public vocabulary, likely in `tina::capacity`.

Shape:

```rust
pub trait CapacityWeight {
    fn capacity_weight(&self) -> usize;
}
```

Rules:

- no automatic `size_of::<T>()` fallback;
- weight means user-defined cost, often bytes;
- reports call it weight, not memory;
- weight full is distinct from count full.

## Rock 2: One Weighted Surface

Pick one real surface where count lies.

Preferred first target: HTTP body bytes, because body pressure is already
real and user-visible.

Acceptable alternative: bridge response/request payloads if HTTP is too
messy.

Proof:

- small payload fits;
- oversized payload rejects with weight reason;
- high-water weight is reported;
- current weight returns to zero after read/drop/cancel/close;
- count full and weight full are separate facts.

## Rock 3: Shard-Local Shared Scope

Add one shard-local shared weight budget.

Shape can be smaller than this, but must be explicit:

```rust
let scope = runtime.register_capacity_scope_on(
    shard,
    CapacityScopeConfig::weight("http.bodies", limit),
)?;
```

Rules:

- no cross-shard scope;
- no hidden global;
- scope has name, shard, max, current, high-water, full count;
- claim/release is owned by the runtime/bridge/surface, not user memory;
- release happens on dequeue/drop/cancel/close.

Proof:

- two surfaces are each under local cap;
- combined weight fills shared scope;
- rejection names shared scope;
- after release, new work admits.

## Rock 4: Unbounded For Now

Add explicit unbounded discovery mode.

Required:

- reason string;
- default live expiry: 1 hour;
- tests use tiny expiry;
- appears in capacity summary;
- production policy rejects;
- expiry failure names surface, reason, observed high-water, and next action.

Ugly escape hatch:

```rust
unbounded_without_expiry_i_know_this_is_bad(reason)
```

Rules:

- rejected by test/prod by default;
- warning/event always visible where validation/reporting exists;
- searchable ugly name is intentional.

Do not spread unbounded everywhere. One surface is enough.

## Rock 5: Assertions And Discovery

Extend capacity assertions so tests can say:

```rust
summary.surface("pool.waiters").no_full();
summary.surface("orders.mailbox").high_water_at_most(96);
summary.scope("http.bodies").weight_high_water_at_most(64 * MIB);
```

Need both:

- `Result` API for tools/sweeps;
- assert wrappers for tests.

Discovery formatter should show:

- name;
- mode;
- fixed/tuning/unbounded;
- count cap/current/high/full;
- weight cap/current/high/full;
- suggested next action.

## Rock 6: Sim/DST Proof If Touched Surface Has Sim

If the chosen weighted/shared surface exists in `tina-sim`, add replay
summary assertions.

Minimum:

- same seed/config/history gives same capacity summary;
- changed cap changes failure or report intentionally.

If live-only, say so in this plan and add normal runtime tests instead.

## Rock 7: Docs And One Specimen

Update docs with the workflow:

```text
unknown -> measured -> fixed
```

Say plainly:

- count protects turns;
- weight protects user-declared payload cost;
- shared scope protects a group;
- unbounded is temporary and loud;
- huge caps are not design.

Update one specimen that naturally shows this:

- HTTP body/backpressure specimen, preferred; or
- DB/bridge/pool specimen if HTTP is not the implementation target.

The specimen should emit or assert a capacity summary.

## Proof Targets

- weight trait/report unit tests;
- weighted surface small/too-heavy/high-water/reclaim tests;
- shared-scope aggregate-full/release/refill tests;
- unbounded expiry test;
- policy rejects unbounded in prod;
- assertion helper tests;
- sim/DST capacity test if available;
- one specimen smoke test.

## Stop Signs

Stop and report before broadening scope if:

- the design wants process-global budgets;
- exact heap memory measurement appears;
- every surface needs conversion to prove one idea;
- user-owned manual release becomes the common path;
- `Full` gets hidden behind automatic retry.

## Landing Criteria

- One real weighted capacity works.
- One shared shard-local scope works.
- Unbounded is explicit, expiring, and policy-rejected.
- Reports distinguish count, local weight, and shared weight.
- Capacity current returns to zero after cancellation/close/drop for touched
  surfaces.
- Docs teach how to tune from evidence.
