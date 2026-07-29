# Boundedness And Overload

Tina should make overload boring and visible.

Important words:

- accepted
- full
- closed
- timeout

If a system is under pressure, it should shed load explicitly instead of
quietly growing hidden queues.

## Start Here: The Budget Manifest

A real service has many caps: mailboxes, pools, body bytes, lanes,
protocol sessions, bridge in-flight. Scattered through handlers and
`register_*` calls, they are easy to lose and easy to guess. Declare
them once in a `ServiceBudgetManifest` instead.

```rust
use tina_runtime::budget::{
    BudgetCap, BudgetKind, BudgetSurface, BudgetUnit, ServiceBudgetManifest,
};
use tina::capacity::CapacityPolicy;

let mut manifest = ServiceBudgetManifest::new("billing", CapacityPolicy::Production)
    .require_kind(BudgetKind::Mailbox)
    .require_kind(BudgetKind::BodyBytes);

manifest.add(
    BudgetSurface::new(
        "billing.controller.mailbox",
        BudgetKind::Mailbox,
        BudgetUnit::Messages,
        BudgetCap::fixed(32),
    )
    .owned_by("controller"),
);
manifest.add(
    BudgetSurface::new(
        "billing.http.request_body",
        BudgetKind::BodyBytes,
        BudgetUnit::Bytes,
        BudgetCap::fixed(64 * 1024),
    )
    .owned_by("listener"),
);

// Fails before any socket binds, with typed errors — never a panic
// and never a silent fallback.
manifest.validate().expect("real caps validate");
```

Then read the caps back from the manifest at each call site instead of
re-typing a literal:

```rust
let body_cap = manifest.cap_max("billing.http.request_body").unwrap();
```

`validate()` rejects the mistakes that bite in production:

- duplicate or whitespace-bearing surface names;
- a zero cap (it would deadlock a queue, fake EOF on a byte budget, or
  disable a rail — never a real budget);
- an `Unbounded` cap under `CapacityPolicy::Production`, or an expired
  `unbounded_for_now`;
- a printable field that looks like a secret value (env var *names* and
  file paths are fine; a `password=...`, a credential URL, an AWS key
  id, or a PEM block is rejected);
- a missing `require_kind` row a copied skeleton must carry.

### Build rows from the configs you already have

You do not hand-type every row. The config structs emit their own:

```rust
manifest.extend(local_system_config.budget_surfaces("runtime"));
manifest.extend(http_server_config.budget_surfaces("http"));
manifest.extend(sqlite_config.budget_surfaces("db"));
```

The mapping is exact: one config field, one row. Adapters describe what
the config already says; they never invent a cap. Time deadlines
(`service_call_timeout`, `request_timeout`, …) are deliberately *not*
surfaced — the unit vocabulary is count and weight, not time, so a
deadline stays plain config rather than a faked count.

### Join the manifest with what actually happened

Pass the live [`ServicePressureReport`](17-pressure-report-convention.md) to
`manifest.report(...)` to get one object answering "what did I
configure, what was used, what was full?". The configured caps come from
the manifest; the observed `cur` / `high` / `full` numbers come from the
runtime report, never the other way around.

```rust
let report = manifest.report(&live_pressure);
println!("{}", report.summary_line());
// budget service=billing schema=1 surfaces=8 observed=2 full=true consistent=true
assert!(report.consistency.is_consistent()); // every live surface has a row
```

`compare_capacity_summary` and `compare_service_pressure` return typed
rows — `Missing`, `Extra`, `CapMismatch`, `UnitMismatch`, `ModeMismatch`
— so a manifest that drifts from the live surfaces fails a test, not a
3am page.

### Pin replay-affecting caps

Mark each surface `ReplayAffecting` (the default) or `display_only()`.
`manifest.replay_export()` hashes only the replay-affecting caps:

```rust
let export = manifest.replay_export();
// Pin export.replay_affecting_hash into a saved DST case. Changing a
// replay-affecting cap changes the hash; changing a display-only cap
// (e.g. an accept-queue depth) does not, and the export names what it
// ignored.
```

This keeps a saved replay case from silently riding ambient defaults:
if a body cap that the case depends on changes, the hash changes too.

## Mailbox Capacity

Every isolate mailbox has capacity. The capacity is one number, but
it has to absorb two distinct streams of messages:

> total capacity = inbound messages + replies to outstanding runtime calls

If an isolate has 4 inbound senders and runs 2 `tcp_read`s in flight
at once, both peaks have to fit. A capacity of `4` does not reject
the read replies: runtime-call continuations are never dropped on a
full mailbox. The continuation parks in the isolate's priority
overflow lane and the trace shows `CallContinuationOverflowed`; the
overflow drains ahead of ordinary mailbox traffic and the call still
completes. Under-sizing costs ordering and latency, not the reply.

Use [`MailboxBudget`](https://docs.rs/tina-runtime) at the spawn site
to make the math obvious:

```rust
use tina_runtime::MailboxBudget;

let cap = MailboxBudget::session(
    /* inbound peer messages */ 4,
    /* in-flight runtime calls */ 2,
).total();

spawn(ChildDefinition::new(child, cap))
```

Presets for common shapes:

- `MailboxBudget::listener(max_in_flight_accepts)` — listener that
  re-arms one `tcp_accept` per ready peer.
- `MailboxBudget::session(in_flight_peer_messages, in_flight_runtime_calls)`
  — TCP/HTTP session isolate.
- `MailboxBudget::service(concurrent_requests, in_flight_runtime_calls)`
  — request/reply service.
- `MailboxBudget::fanout(upstream_callers, worker_count)` — frontend
  that fans out to N workers; each worker reply needs a slot.

If capacity is `1`, the second queued message should hit pressure.

## Pressure Diagnostics

`LocalSystem::pressure_summary()` and the lower-level
`ThreadedRuntime::pressure_summary()` walk the trace and return a counted
summary:

```rust
let summary = runtime.pressure_summary()?;
assert_eq!(summary.completion_rejected_mailbox_full, 0);
println!("pressure: {summary}");
// pressure: completion[mbox_full=0 ...] reply[no_pending=0 ...] send[full=0 closed=0]
```

`summary.any_full()` is the one-line "did we hit a *Full* anywhere?"
check. The closed/no-pending counts reflect lifecycle, not capacity,
so they're broken out separately.

## Bound Producer Work Before Effects Exist

The review question for every fanout loop is:

```text
what is the max in-flight work, and did the service choose it?
```

Use a service-owned wrapper before effects exist:

```rust
use tina_runtime::{BoundedItems, bounded_batch};

let items = match BoundedItems::try_from_iter(self.max_items_per_request, request.items) {
    Ok(items) => items,
    Err(_) => return reply(Reply::TooManyItems),
};

let effects = items.map_effects(|item| {
    call(worker, WorkerMsg::Run(item), timeout).then(Msg::Done)
});

bounded_batch(effects)
```

`BoundedItems` and `BoundedEffects` are small rails, not magic. They reject
zero caps and stop at the first over-cap item/effect. They preserve order.
Prefer `BoundedItems::map_effects(...)` when a request list is the source of
the work: the list is capped before any per-item effect is constructed.
Tests can pin the contract with:

```rust
tina_runtime::assert_service_owned_bound(
    "orders.batch.items",
    Some(config.max_items_per_request),
    Some(report.items_observed),
)?;
```

Reserve raw `batch(...)` for small, statically bounded collections. This is
the hazardous shape for request data because the request chooses how many
effects exist:

```rust
let effects = request
    .items
    .into_iter()
    .map(|item| call(worker, WorkerMsg::Run(item), timeout).then(Msg::Done))
    .collect::<Vec<_>>();
batch(effects)
```

## Observed Send

Plain `send` is simple but does not tell sender what happened. First bound a
request-sized burst as shown above; then use observed send when each item's
overload result matters:

```rust
use tina_runtime::{BoundedItems, SendOutcome, bounded_batch, send_observed};

#[derive(Debug, Clone)]
enum ProducerMsg {
    Burst(BoundedItems<usize>),
    Sent(SendOutcome),
}

#[tina_runtime::isolate(
    message = ProducerMsg,
    send = Outbound<ConsumerMsg>,
    shard = AppShard
)]
impl Producer {
    fn handle(&mut self, msg: ProducerMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ProducerMsg::Burst(items) => {
                bounded_batch(items.map_effects(|i| {
                    send_observed(self.consumer, ConsumerMsg::Item(i))
                        .then(ProducerMsg::Sent)
                }))
            }
            ProducerMsg::Sent(outcome) => {
                if matches!(outcome, SendOutcome::Full) {
                    self.rejected += 1;
                }
                noop()
            }
        }
    }
}
```

Construct `ProducerMsg::Burst` with `BoundedItems::try_from_iter` at the
admission boundary. In a request handler, return a typed application reply on
error as in the first example; an event producer can handle the typed
`BoundedItemsError` directly. The event handler cannot accidentally receive an
unbounded raw collection.

This is the heart of Tina service shape.

Many async systems accept into a channel or spawned task until pressure appears
somewhere else.

Tina should be able to say:

```text
accepted=12000 full=38000 timeouts=0 exit=clean
```

## Bounded Broadcast

When one event goes to many sessions, do not build effects from a raw
request-sized `Vec`. Build a `BroadcastTargets` first. The service chooses
`max_targets`, and anything past that cap is refused before it can become
runtime work.

```rust
use tina_runtime::{BroadcastTargets, BroadcastTracker, broadcast_observed};

enum RoomMsg {
    Publish { body: Bytes },
    Delivered(SessionId, SendOutcome),
}

struct Room {
    subscribers: Vec<(SessionId, Address<SessionMsg>)>,
    max_broadcast_targets: usize,
    broadcast: Option<BroadcastTracker<SessionId>>,
}

RoomMsg::Publish { body } => {
    let targets = match BroadcastTargets::try_from_iter(
        self.max_broadcast_targets,
        self.subscribers.iter().map(|(id, addr)| (*id, *addr)),
    ) {
        Ok(targets) => targets,
        Err(_) => return self.reply_full(),
    };
    self.broadcast = Some(targets.tracker());
    broadcast_observed(
        targets,
        |_| SessionMsg::Deliver(body.clone()),
        RoomMsg::Delivered,
    )
}

RoomMsg::Delivered(id, outcome) => {
    if let Some(report) = self.broadcast.as_mut().unwrap().record(id, outcome).unwrap() {
        assert!(report.assert_all_accounted_for(report.outcomes().len()).is_ok());
        // report.accepted(), report.full(), report.closed()
    }
    noop()
}
```

The helper is intentionally small. It does not own rooms, retries, or
session cleanup. It gives the service a bounded target list, one ordinary
continuation per target, and one report that accounts for
`Accepted` / `Full` / `Closed`.

## Bounded Pools

When the bounded thing is "borrow one of N resources, do work, give
it back," reach for `tina_runtime::pool::WorkerPool`. It bounds two
quantities — resource count (`PoolConfig::capacity`) and parked
waiters (`PoolConfig::max_waiters`) — and surfaces overflow as the
typed outcomes above (`AcquireOutcome::Full` /
`AcquireOutcome::Closed`). Caller cancellation reclaims waiter
capacity; the pressure report exposes `full_count`, `cancel_count`,
`closed_count`, `retired_count`, and `dispatch_recovered` for
operator dashboards. Don't open-code a worker frontend with a
`PendingReplies` table when you actually want a borrow/return
lifecycle. See the [ergonomics
checklist](./11-ergonomics-checklist.md#bounded-worker-pool).

## Admission And Rate Policy

When the bounded mailbox is the *only* overload signal, every overload
story collapses to the same shape: a `Full` reply somewhere. Real edge
services need more vocabulary — "shed", "wait", "rate-limit", "degrade",
"close" — and a typed outcome for each. `tina_runtime::admission` ships
three small policy types that compose with everything above.

```rust
use tina_runtime::{
    AdmissionDecision, ConcurrencyLimit, KeyedLimit, PressureAction, RateLimit,
    RateLimitConfig, RateLimitDecision, ServicePolicy,
};
```

Three policies, with decision shapes matched to their configuration:

- `ConcurrencyLimit` — fixed-cap local concurrency. Returns a move-only,
  gate-identified `ConcurrencyPermit`.
- `KeyedLimit<K>` — fixed-cap per-key concurrency. Move-only
  `KeyedPermit<K>`. Per-key storage is a `Vec<Option<...>>`; nothing
  grows.
- `RateLimit<K>` — replayable per-key token bucket. Decisions are pure
  functions of `(rate, burst, now, key history)`. The admission owner supplies
  `now` from `ctx.now()` or `call.now()`.

Generic `ServicePolicy` code returns `AdmissionDecision<T>`:

```rust
match policy.decide(key, now) {
    AdmissionDecision::Admitted(grant) => /* charge held */,
    AdmissionDecision::RateLimited { retry_after, .. } => reply_limited(retry_after),
    AdmissionDecision::Full(_) => reply_full(),
    AdmissionDecision::Closed(_) => reply_closed(),
    AdmissionDecision::Wait { delay, .. } => /* compose with SharedWork */,
    AdmissionDecision::Degrade { .. } => reply_degraded(),
    AdmissionDecision::TimedOut(_) => reply_timeout(),
}
```

`RateLimit::try_admit_at` uses a narrower decision so handlers only match
outcomes the token bucket can produce. The `_at` suffix makes the explicit
logical-time authority visible:

```rust
let mut rate = RateLimit::new(
    "gateway.rate",
    RateLimitConfig {
        max_keys: 1_000,
        rate_per_sec: 10,
        burst: 20,
    },
);

match rate.try_admit_at(&tenant, call.now()) {
    RateLimitDecision::Admitted => serve(tenant),
    RateLimitDecision::RateLimited { retry_after, .. } => {
        reply_limited(retry_after)
    }
    RateLimitDecision::KeyCapacityFull(_) => reply_full(),
    RateLimitDecision::Closed(_) => reply_closed(),
}
```

`Admitted` carries no permit because its token has already been consumed;
there is no authority or capacity to release. `RateLimit` reports tracked-key
pressure as `KeyCapacityFull`; it has no hidden wait, degrade, or
pressure-triggered close configuration. Explicit `close()` still produces
`Closed`. Generic code can drive it through `ServicePolicy::decide`, which
deliberately widens these four outcomes into `AdmissionDecision<()>`.

Rules the layer keeps:

- **No hidden retry.** `RateLimit` returns a deterministic `retry_after`;
  the caller decides whether to sleep and try again. Pair with
  `FullHandling::retry_backoff(...)` if you want a retry budget — the
  budget is explicit, exhaustion is typed.
- **No invisible queue.** Bounded wait is a *decision shape*, not a new
  waiter product. Use `SharedWork` for the actual wait.
- **Capacity report integration.** Every policy projects onto
  `CapacitySurfaceReport`; rejection counts roll into `full_count` so
  `summary.any_full()` stays honest for admission surfaces.
- **Fixed-capacity per-key storage.** No growing `HashMap`. A new rate-limit
  key finds a free slot or sees `KeyCapacityFull(report)`; broad admission
  policies use `Full(report)`. Neither path silently evicts.
- **Move-only proofs.** `ConcurrencyPermit` / `KeyedPermit` cannot be released
  twice. The compile-fail tests prove it.

`examples/systems/system_tenant_rate_limiter` is the motivating R&D proof. Its
gateway owns timestamp authority through `call.now()`; callers cannot mint
refill credit by supplying an admission timestamp.

## Shared Weighted Budgets

When one request consumes more than one shard-local budget, do not
hand-roll a rollback chain. Use `SharedCapacityReservation`.

```rust
let reservation = match SharedCapacityReservation::try_reserve([
    in_flight.charge(route_weight),
    body_bytes.charge(request_len),
]) {
    Ok(reservation) => reservation,
    Err(full) => return call.reply(Reply::Full {
        surface: full.scope,
        requested: full.requested,
        current: full.current,
        max: full.max,
    }),
};
```

If any charge is full, already-acquired charges are released before
`try_reserve` returns. If the returned reservation is parked in
`GuardedPendingReplies`, dropping the slot on reply, drain, or
caller-gone sweep releases every charge exactly once.

Use this when the user story is "one request costs N units from several
shared surfaces": body bytes plus in-flight work, tenant work plus
global work, or pool waiters plus per-route admission.

## Timeout Is Load Control

Request/reply uses timeout:

```rust
call(worker, WorkerMsg::Run(job), Duration::from_millis(20))
    .then(ClientMsg::Done)
```

Handle timeout as normal behavior.

Do not panic on timeout in service code.

## Measurement Rule

When testing a Tina service under load, collect:

- accepted work
- rejected full
- closed
- timeouts
- peak pending if easy
- exit status
- crude RSS if the platform gives it

First pass can run without hard memory caps. Later pressure passes can use
Linux/Fly/Docker limits.

## Capacity Is Not A Guess

Bounded queues need numbers. Do not write `usize::MAX`. Do not
write 10_000. Pick the number this way:

```text
unknown -> measured -> fixed
```

### The four steps

1. **Pick a number.** Mark it `Tuning`. The cap is still hard. The
   flag just says "report high water loudly".
2. **Run the load.** Real workload, not a smoke test.
3. **Read the high water** off a `CapacitySurfaceReport`.
4. **Freeze a `Fixed` cap** a bit higher than high water. Common
   choice: `2 * high_water` for elastic loads, `1.2 * high_water`
   for steady loads.

### Types

`tina::capacity`:

- `CapacityMode::Fixed` — measured cap. Use in production.
- `CapacityMode::Tuning` — discovery cap. Still hard. The flag
  surfaces high water loudly.
- `CapacityMode::UnboundedForNow { reason, expires_at }` —
  temporary unbounded mode. The helper
  `CapacityMode::unbounded_for_now(reason)` expires in one hour.
  Production rejects it.
- `CapacityMode::unbounded_without_expiry_i_know_this_is_bad(reason)`
  — ugly escape hatch, development-only by default.
- `CapacityPolicy::{Development, Test, Production}` — validates
  which modes pass.
- `CapacitySurfaceReport` — one snapshot per bounded surface:
  name, mode, count cap/current/high/full, optional weight
  cap/current/high/full, and optional shard-local shared-scope
  weight fields.

Count is number of things. Weight is user-declared cost. Tina does
not infer heap memory, and discovery lines should not be read as
exact allocator claims.

`tina-runtime`:

- `CapacitySummary` — collects reports, looks up by name.
- `SurfaceAssertion` — `.no_full()`, `.high_water_at_most(N)`,
  `.full_count_eq(N)`. Returns `Result`.
- `format_discovery_line(&report)` — one `key=value` line.

### Step 3 in code

Two surfaces report today. Each has its own way to read the
report.

**`PendingReplies`** lives in user code. Call
`capacity_report()` directly:

```rust
let pending = PendingReplies::<u64, MyReply>::with_capacity(MAX)
    .named("orders.pending")           // pin for CI
    .with_capacity_mode(CapacityMode::Tuning);

// ...later, in handler scope...
let report = self.pending.capacity_report();
```

If a different isolate needs the report, add a snapshot message
to the holder isolate:

```rust
enum FrontendMsg {
    // ...other variants...
    CapacitySnapshot,
}

enum FrontendReply {
    // ...other variants...
    Capacity(CapacitySurfaceReport),
}

// In handle:
FrontendMsg::CapacitySnapshot => {
    reply(FrontendReply::Capacity(self.pending.capacity_report()))
}
```

**`WorkerPool`** is owned by the runtime once registered. It
already has a `PressureReport` message; project the reply onto a
capacity surface:

```rust
let surface = pool_pressure.to_waiters_capacity_report(
    "pool.orders.waiters",       // caller picks the name
    CapacityMode::Tuning,
);
```

### Step 4 in code

```rust
let mut summary = CapacitySummary::new();
summary.push(surface)?;

// Print one line per surface.
println!("{}", format_discovery_line(
    summary.surface("pool.orders.waiters").report()?
));

// Or assert in CI.
summary.surface("pool.orders.waiters").no_full()?;
summary.surface("pool.orders.waiters").high_water_at_most(96)?;
```

Output of `format_discovery_line` (same shape as
`format_pressure_line`):

```text
capacity surface=pool.orders.waiters mode=tuning max=4  cur=0 high=4  full=0 suggest="tuning cap is tight; raise then re-measure"
capacity surface=orders.pending      mode=fixed  max=64 cur=0 high=11 full=0 suggest="fixed cap is loose; consider shrinking"
```

The `suggest=` hint is advice. Read it, decide, freeze.

### Naming rule

- Default name is fine for human reports.
- Pin an explicit name with `.named(...)` (PendingReplies) or by
  passing the name to `to_waiters_capacity_report` (WorkerPool)
  whenever a CI test asserts on that surface. A refactor that
  reorders construction or renames internals must not silently
  retarget the assertion.
- Use a dotted token form (e.g. `pool.orders.waiters`).
  `CapacitySummary::push` rejects empty names and names with
  whitespace or control characters so the discovery line stays
  parseable.

### Worked example

`examples/specimen_pool_cancel_reclaim` runs the full loop:
configure pool with `Tuning`, drive load, pull `PressureReport`,
project to a capacity surface, format a discovery line, assert
on it.

`examples/specimen_http_body_streaming` shows the weighted form on
HTTP response bodies:

```text
unknown: pick a body-byte cap and run the slow-reader workload
measured: read high_weight=4096 from the discovery line
fixed: keep the cap near one chunk for this streaming route
```

Its Tina side emits a weighted line like:

```text
capacity surface=specimen_http_body_streaming.response_body mode=fixed max=- cur=0 high=0 full=0 suggest="weighted cap fits" weight_unit=bytes max_weight=4096 cur_weight=0 high_weight=4096 weight_full=0 shared_scope=http.bodies shared_max_weight=262144 shared_cur_weight=0 shared_high_weight=4096 shared_weight_full=0
```

Request and response body reports can share the same `http.bodies`
scope, but that scope is shard-local: one `BodyMetrics` instance
threaded through one listener and its connection isolates.

## Turn Overload Into A Bugbox

When overload happens in a live run, save the bounded facts, not the
vibes:

```rust
use tina_sim::dst::{
    assert_no_hidden_buffering, assert_overload_visible, capture_overload_run,
    save_overload_bug,
};

assert_no_hidden_buffering(&capacity_report);
assert_overload_visible(&capacity_report);

let capture = capture_overload_run("slow peer filled broadcast targets")
    .with_seed(seed)
    .with_config(replay_config)
    .with_scenario("one slow peer should not create hidden fanout")
    .with_history(ops)
    .with_invariant("broadcast overload is visible as Full")
    .with_trace(&live_trace)
    .with_capacity_summary(&capacity_report)
    .finish()?;

let saved = save_overload_bug("cases/slow-peer.case", &capture, |op| op.to_string())?;
eprintln!("{saved}");
```

The replay side must reproduce the same capacity fact with
`replay_overload_bug(...)`. If the live run contains a protocol or
external-bridge fact the simulator cannot model yet, record it as an
unsupported fact. Replay then fails closed instead of pretending the case
is deterministic.

## What Counts As Failure

Good failure:

```text
server says full
client gets response
process stays alive
metrics make sense
```

Bad failure:

```text
process OOMs
latency goes strange
no overload signal exists
shutdown hangs
metrics lie
```

When Tina fails badly, write it down. That is the work.
