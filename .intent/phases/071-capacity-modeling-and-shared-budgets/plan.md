# Phase 071: Capacity Modeling And Shared Budgets

## Status

- Done:
  - PR 1 (count + policy): `tina::capacity` vocabulary
    (`CapacityMode`, `CapacityPolicy`, `CapacityPolicyError`,
    `CapacityFull`, `CapacitySpec`, `CapacitySurfaceReport`),
    `tina-runtime::capacity` collection + assertions
    (`CapacitySummary`, `SurfaceAssertion`, `format_discovery_line`,
    `format_discovery_report`), high-water/full reporting on
    `WorkerPool` waiters and `PendingReplies` slots (with `.named()`
    + `.with_capacity_mode()` builders), `PoolPressureReport`
    extended with `high_water_waiters` and a
    `to_waiters_capacity_report` adapter, tuning-mode hard-cap
    proof, `specimen_pool_cancel_reclaim` updated to emit + assert
    a discovery line, user-guide section showing the
    `unknown -> measured -> fixed` workflow.
- In progress:
- Open:
  - whether unbounded expiry uses live time, sim steps, or both in first form
    (PR 2);
  - whether shared scope is one type per shard or per shard-id-keyed registry
    (PR 2).
- Deferred:
  - weight, unbounded-for-now, ugly no-expiry escape, shard-local shared scope,
    sim/DST capacity assertions, weighted specimen/doc — all PR 2;
  - process-wide global capacity pools;
  - exact heap-memory accounting;
  - automatic recursive payload sizing;
  - production policy UI beyond config validation.

## Rock 0 Decisions (PR 1)

### API home

- Vocabulary (data types, no runtime state) lives in **`tina::capacity`**:
  `CapacityMode`, `CapacityPolicy`, `CapacityPolicyError`, `CapacityFull`,
  `CapacitySurfaceReport`, `CapacityScopeReport` (PR 2),
  `CapacityWeight` trait (PR 2). Pure data, derives `Debug`/`Clone`/`PartialEq`
  where it makes sense, no runtime types reachable from `tina`.
- Runtime-side collection and assertion helpers live in **`tina-runtime::capacity`**:
  `CapacitySummary` (a list of `CapacitySurfaceReport`s with name lookup),
  `SurfaceAssertions` (returned by `summary.surface(name)`), and a
  one-line discovery formatter.
- Sim/DST capacity assertions (`case.capacity(name).full_count_eq(...)` etc.)
  land in **`tina-sim`** in PR 2 alongside the simulator-backed surface.
- Bridge/specimen adapters never define new capacity vocabulary — they emit
  `CapacitySurfaceReport`s from existing trackers.

Reasoning: the report shape is the API; runtime collection is mechanism.
Keep them in different crates so a future weighted/shared-scope surface
that has no runtime state (e.g. a config printout) can still produce a
`CapacitySurfaceReport` without depending on `tina-runtime`.

### Naming rule

- Each bounded surface carries an **optional explicit name** plus a
  **stable default name** generated at construction. Defaults are good
  enough for human reports and pressure dashboards; explicit names are
  the blessed shape for CI / DST assertions because refactors that
  rename internals must not break tests by accident.
- Constructor surface (PR 1, count surfaces only):
  - `WorkerPool::new(config, resources)` — default name
    `"pool.<pool_id>.waiters"`.
  - `PendingReplies::with_capacity(cap)` — default name
    `"pending_replies.<auto_seq>"`.
  - Both add `.named("<dotted.name>")` builder.
- Default names use a per-surface monotonic counter so the n-th
  `WorkerPool` constructed in a process is `pool.<n>.waiters`. Stable
  enough for pressure dashboards; CI/DST tests must pin explicit names.
- Duplicate **explicit** names within one summary: `CapacitySummary::push`
  rejects with `Err(CapacityNameError::Duplicate(name))` so a test never
  silently picks the wrong surface.

Reasoning: forcing names everywhere would be ceremony. Forcing names for
CI assertions is necessary so refactors do not silently retarget an
assertion. Generated defaults give pressure-dashboard truth for free.

### Tuning still has a hard cap

`CapacityMode::Tuning` is `Fixed`-with-a-loud-flag, not "unbounded
in disguise". The cap is a real upper bound; tuning only signals
intent ("this number was chosen for discovery, please report
high water loudly") so docs / dashboards / discovery formatter can
say "read the high-water and freeze". Unbounded modes are PR 2.

### PR 1 surfaces

- `WorkerPool`: count cap on waiters. Reports `current=live waiters`,
  `high_water=highest live waiters since construction`, `full_count`
  reuses existing `PoolPressureReport.full_count`.
- `PendingReplies`: count cap on slots. Already has `len()`,
  `high_water()`, and `full_rejects()`; just needs a name and a
  `capacity_report()` adapter.

Both surfaces gain `.named(...)` builder + a `capacity_report()`
method returning `CapacitySurfaceReport`. The mode (`Fixed` or
`Tuning`) is carried on the surface itself so `capacity_report()`
can stamp it; the constructors accept a `CapacitySpec` (= cap +
mode + optional name) instead of a bare `usize` going forward.
Bare `usize` constructors stay for back-compat and default to
`Fixed`.

## Grug Truth

Capacity is not a big number guessed once.

```text
unknown -> measured -> fixed
```

Count protects scheduler fairness.

Weight protects memory-ish user payload.

Shared weight protects a group of queues.

Unbounded is a bomb with label and timer.

Every `Full` says which cap filled.

No hidden globals.

## Problem

Tina already makes queues bounded, but count-only caps invite fake safety:

```rust
mailbox_capacity = 10_000
```

That says little about memory. Ten thousand tiny ticks and ten thousand
large HTTP bodies are different systems.

Users also do not always know the right cap up front. If Tina only accepts
fixed counts, people will write a huge number and move on. Better: make
capacity discovery cheap, visible, and testable.

## Goal

Make capacity a first-class, inspectable, testable part of Tina.

First form:

- one shared report shape for bounded surfaces;
- at least two count surfaces report current, high-water, and full count;
- capacity reports are cheap boring structs;
- tuning mode helps measure unknown caps;
- weighted capacity is explicit user-declared cost, not fake heap memory;
- capacity policy validates dev/test/prod rules;
- unbounded-for-now is loud, searchable, expiring, and rejected by prod profile;
- shard-local shared capacity scopes let related queues share one aggregate
  budget without process-global magic;
- sim/DST can assert capacity summaries under replay;
- surfaces not converted yet get named TODOs, not silent non-coverage.

This phase should not stop at a toy. The smallest surface proves the model;
then the phase locks in the bigger direction. Two PRs max:

1. count reports, tuning mode, policy, and capacity assertions;
2. weight, unbounded modes, shard-local shared scope, and specimen/DST proof.

## Non-Goals

- exact allocator memory accounting;
- automatic `Vec` / `String` / map traversal;
- process-wide global memory manager;
- generic benchmark harness;
- hidden retry or hidden queue;
- making `Full` disappear.

## Vocabulary

### Count

Number of things.

Examples:

- mailbox messages;
- pending replies;
- pool waiters;
- worker resources;
- driver lane slots.

### Weight

User-declared cost.

Often bytes. Sometimes rows, jobs, handles, tenants, packets, CPU-ish work,
or estimated payload size.

Call it **weight**, not memory. Tina does not know heap truth.

Candidate trait:

```rust
pub trait CapacityWeight {
    fn capacity_weight(&self) -> usize;
}
```

If a user asks for weighted capacity, the message/payload type must provide
weight. No silent `size_of::<T>()` fallback for heap-owning payloads.

Default capacity is count. Weight is opt-in and user-defined.

If the user lies or guesses badly about weight, Tina cannot save them from
unbounded real memory use or annoying early `Full`/expiry failures. Tina can
make the cause obvious: current weight, high-water weight, configured limit,
and the surface/scope that filled.

### Shared Scope

A named budget used by multiple bounded surfaces on one shard.

Example:

```text
shard-2.http.bodies <= 128 MiB weight
```

Local cap protects one queue. Shared cap protects the group.

First form is shard-local only. Cross-shard shared capacity is out of scope.
If a user needs global budget, build a capacity service explicitly.

## Candidate API

Names are not final.

Names should not become ceremony. Registration context should provide a
boring default name. Users can override it for better reports.

Default names must be stable enough for reports. For CI assertions, prefer
explicit `.named(...)` so refactors do not break tests by accident.

```rust
MailboxBudget::fixed(128)
MailboxBudget::fixed(128).named("orders.mailbox")

MailboxBudget::tuning(256)
MailboxBudget::tuning(256).named("orders.mailbox")

MailboxBudget::weighted::<OrderMsg>(128, 512 * KiB)
MailboxBudget::weighted::<OrderMsg>(128, 512 * KiB).named("orders.mailbox")

MailboxBudget::unbounded_for_now("admin import prototype")

MailboxBudget::unbounded_without_expiry_i_know_this_is_bad(
    "temporary internal batch runner; tracked in issue #123",
)
```

Shared scope:

```rust
let bodies = runtime.register_capacity_scope_on(
    shard,
    CapacityScopeConfig::weight("http.bodies", 128 * MiB),
)?;

let budget = MailboxBudget::messages(128)
    .shared_weight::<HttpMsg>(bodies);
```

This is only a candidate. Keep the shipped shape smaller if needed.

Use `tuning` or another word that says "hard cap, but please report high
water." Do not ship a name that sounds passively unbounded.

Capacity policy:

```rust
CapacityPolicy::Development
CapacityPolicy::Test
CapacityPolicy::Production
```

Policy decides whether tuning/unbounded modes are allowed:

| Mode | Dev | Test | Prod |
|---|---|---|---|
| `Fixed` | allow | allow | allow |
| `Tuning` | allow | allow | allow or warn |
| `UnboundedForNow` | allow with expiry | allow with expiry | reject |
| `UnboundedWithoutExpiry` | warn loudly | reject by default | reject |

Production may allow tuning caps because early production is sometimes
where real high-water data appears. Production must reject unbounded unless
the user opts into an even louder deployment escape hatch.

## Reporting

Capacity report first form:

```rust
pub struct CapacitySurfaceReport {
    pub name: String,
    pub mode: CapacityMode,
    pub max_messages: Option<usize>,
    pub current_messages: usize,
    pub high_water_messages: usize,
    pub full_count: u64,
    pub max_weight: Option<usize>,
    pub current_weight: Option<usize>,
    pub high_water_weight: Option<usize>,
    pub weight_full_count: u64,
}

pub struct CapacityScopeReport {
    pub shard: ShardId,
    pub name: String,
    pub max_weight: usize,
    pub current_weight: usize,
    pub high_water_weight: usize,
    pub full_count: u64,
}
```

A `Full` caused by capacity must say why:

```rust
CapacityFull::LocalMessages
CapacityFull::LocalWeight
CapacityFull::SharedWeight { scope: CapacityScopeId }
```

Do not collapse all pressure into generic `Full` in reports.

Reports should answer:

```text
what cap did I set?
how close did I get?
how often did I hit it?
was pressure count, local weight, or shared weight?
did it recover to zero?
```

## Unbounded For Now

Unbounded is allowed only as an explicit discovery tool.

Default behavior:

- requires a reason string;
- emits startup warning;
- appears in every capacity report;
- default live expiry: 1 hour;
- rejected by production validation;
- sim expiry is deterministic logical steps or simulated time;
- expiry emits a fatal capacity event and stops/panics according to profile.

Escape hatch:

```rust
unbounded_without_expiry_i_know_this_is_bad(reason)
```

Rules:

- ugly name on purpose;
- requires meaningful reason;
- always appears in startup warnings and capacity summaries;
- production profile rejects unless explicitly allowed;
- emits `CapacityUnboundedWithoutExpiry`.

Make unbounded easy to fix:

- report observed high water;
- report recommended next action: "run tuning, pick fixed cap";
- show which surface/scope is unbounded;
- keep reason and creation site searchable;
- do not auto-pick the cap for the user.

## Runtime Ownership

Capacity claims must be released structurally.

Good:

```text
runtime charges weight on enqueue/admit
runtime releases weight on dequeue/drop/cancel/close
```

Bad:

```rust
scope.acquire(weight)?;
// user must remember release
scope.release(weight);
```

For runtime-owned queues and bridge buffers, the runtime/bridge owns release.
For user-owned buffers, use an explicit lease if needed.

## DST

Capacity is replay truth.

Simulator state should include capacity claims/releases/full events where the
surface exists:

```text
CapacityClaimed(scope, weight)
CapacityReleased(scope, weight)
CapacityFull(scope, requested, available)
```

Same seed + config + history should produce the same capacity summary.

Saved replay cases may assert:

```rust
case.capacity("pool.waiters").full_count_eq(2);
case.capacity("router.mailbox").high_water_at_most(32);
case.capacity_scope("shard-1.http.bodies").high_water_weight_at_most(64 * MiB);
```

Exact sample sequences are optional. Summary assertions are first form.

## Rocks

### Rock 0: Pin API Home And Names

Before coding, decide where each thing lives:

- public config/report/policy vocabulary;
- runtime collectors;
- sim/DST assertions;
- bridge/specimen adapters.

Write this at the top of the phase artifact. Do not scatter capacity helpers
across crates because the first surface happened to need them there.

Also decide default naming:

- runtime-generated names are fine for human reports;
- explicit `.named(...)` is the blessed shape for CI/DST assertions;
- duplicate names on one shard should reject or disambiguate visibly.

### Rock 1: Audit Bounded Surfaces

List every current bounded surface and whether it can report:

- configured cap;
- current;
- high water;
- full count;
- terminal current == 0 after shutdown when applicable.

Minimum surfaces:

- isolate mailbox;
- `PendingReplies`;
- `WorkerPool` waiters/resources;
- bridge mailboxes and in-flight counts;
- HTTP body/request/response caps;
- SQLite row/request/response caps;
- runtime driver lanes.

Output: short table in the plan artifact and follow-up TODOs for surfaces not
ready.

### Rock 2: Capacity Report Core Types And Policy

Add small report structs in the right crate.

Likely home:

- generic policy/data types in `tina`;
- runtime collection helpers in `tina-runtime`;
- sim assertions in `tina-sim`.

Keep it boring. No exporter. No metrics framework.

Also add policy validation:

- dev/test allow tuning mode;
- prod allows fixed and maybe tuning;
- expiring unbounded is rejected by prod;
- no-expiry unbounded is rejected by test/prod by default;
- validation errors name the surface and bad mode.

### Rock 3: Existing Count Surfaces Report High Water

Wire high-water/full reports for at least two existing count surfaces:

- `WorkerPool`;
- one mailbox or pending-reply surface.

Proof:

- nominal load high-water below cap;
- intentional overload increments full;
- shutdown/cancel reclaims current count;
- report shape appears in pressure summary.

### Rock 4: Tuning Mode

Add a tuning capacity mode.

Tuning still has a hard cap. It only means:

```text
this cap was chosen for discovery
please report high water loudly
```

Docs show the workflow:

```text
run workload
read high water
set fixed cap with safety factor
assert in CI
```

### Rock 5: Unbounded For Now

Add explicit unbounded discovery mode for one surface only.

Hard rules:

- reason required;
- startup warning/event;
- capacity summary names it;
- live default expiry is 1 hour;
- test expiry with a tiny duration;
- production validation rejects it.

Expiry failure text must be useful:

- surface/scope name;
- reason string;
- elapsed time or sim steps;
- observed high water;
- suggested next action.

Do not spread this everywhere until the first form feels right.

### Rock 6: Weight Trait And One Weighted Surface

Add `CapacityWeight` or equivalent.

Default is still count. Weight is opt-in.

The user defines the unit and meaning. Tina records and enforces that
declared weight. Tina does not claim the weight equals heap bytes.

Pick one real surface where count lies:

- HTTP request/response body buffers; or
- bridge response payloads.

Proof:

- two small messages fit;
- one too-heavy message fails with weight reason;
- full reason distinguishes local message count from local/shared weight;
- high-water weight reports.

### Rock 7: Shard-Local Shared Weight Scope

Add first-form shared scope on one shard.

Proof:

- two surfaces share one scope;
- each surface is under local cap;
- aggregate exceeds shared cap;
- rejection says shared scope filled;
- release on dequeue/drop/cancel lowers current weight;
- sim/DST sees same summary.

No cross-shard shared scope.

### Rock 8: Capacity Assertions For Tests

Add tiny test helpers:

```rust
summary.surface("pool.waiters").no_full();
summary.surface("orders.mailbox").high_water_at_most(96);
summary.scope("http.bodies").weight_high_water_at_most(64 * MiB);
```

Helpers return `Result` and have assert wrappers. Do not make users parse
failure strings.

Also add one discovery helper or report formatter that prints:

```text
surface, mode, configured cap, high water, full count, suggested next action
```

This is how users move from unknown to fixed without reading internals.

### Rock 9: Specimen Update

Update one or two specimens where the capacity story matters:

- worker pool / pool cancel reclaim for count capacity;
- native HTTP/HTTPS or outbound HTTP for weight/shared capacity;
- replay DST if sim support lands.

Specimen README should show the tuning path:

```text
unknown -> measured -> fixed
```

## PR Shape

Keep this to at most two PRs.

### PR 1: Count And Policy

Small proof that direction is right:

- report types;
- capacity policy validation;
- high-water/full/current for two count surfaces;
- tuning mode;
- capacity assertion helpers;
- one specimen/doc update.

This PR should make "I do not know the cap yet" feel honest without adding
weight or shared scopes.

### PR 2: Weight And Shared Budgets

Lock in the bigger model:

- `CapacityWeight` or equivalent;
- one weighted surface;
- expiring unbounded and ugly no-expiry escape hatch;
- shard-local shared weight scope;
- sim/DST summary if the touched surface is simulator-backed;
- specimen/doc update showing count + weight + shared pressure.

If PR 2 reveals the PR 1 shape was wrong, change PR 1's public names before
calling the phase landed. Do not leave two competing capacity dialects.

## Proof Targets

- Unit tests for report math and high-water updates.
- Default-name and explicit-name tests.
- Runtime tests for count-cap current/high/full.
- Policy validation tests for dev/test/prod.
- Weighted-cap test for oversized payload.
- Shared-scope test for aggregate full and release.
- Unbounded expiry test.
- Production validation rejects unbounded.
- Sim/replay test if the touched surface is simulator-backed.
- At least one specimen emits or asserts a capacity summary.

## Docs

Add a user-guide page or section:

```text
Capacity is not guess.

Count protects turns.
Weight protects memory-ish payload.
Shared weight protects a group.
Tuning mode measures.
Unbounded is temporary and loud.
```

Docs must say:

- do not use huge numbers as fake design;
- use tuning caps under load;
- freeze caps after observing high water;
- `Full` is good evidence, not a panic;
- exact heap memory is not claimed.

## Out Of Scope

- global process memory manager;
- allocator hooks;
- per-type automatic deep sizing;
- Prometheus/OpenTelemetry export;
- every runtime surface in one pass;
- perfect recommendations like "set cap to X".

## Landing Criteria

- The shipped API makes the common path easier than `usize::MAX`.
- Policy validation is visible before runtime starts.
- At least one count cap and one weight cap report high water.
- Unbounded is explicit, expiring, and rejectable.
- Shared capacity is shard-local and explicit.
- Every new `Full` path names local count, local weight, or shared weight.
- Tests prove capacity is reclaimed after cancel/close/drop for touched
  surfaces.
- Docs teach the workflow.
