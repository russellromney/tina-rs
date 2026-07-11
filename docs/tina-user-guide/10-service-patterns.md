# Service Patterns

A Tina service is usually just an isolate with a reply type.

Service grug:

```text
caller sends one request
service owns state
service may do runtime calls
service eventually replies
caller also has timeout
```

## Basic Service

```rust
#[derive(Debug, Clone)]
enum CounterMsg {
    Add(u64),
}

#[derive(Debug, Clone)]
enum CounterReply {
    Value(u64),
}

struct Counter {
    value: u64,
}

#[tina_runtime::isolate(message = CounterMsg, reply = CounterReply, shard = AppShard)]
impl Counter {
    fn handle(
        &mut self,
        _msg: CounterMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(
        &mut self,
        msg: CounterMsg,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            CounterMsg::Add(n) => {
                self.value += n;
                call.reply(CounterReply::Value(self.value))
            }
        }
    }
}
```

Caller:

```rust
call(counter, CounterMsg::Add(1), Duration::from_millis(20))
    .then(ClientMsg::CounterReturned)
```

## Registration Across Owners

Use the service shape at registration time so callers receive only the lanes
they can use:

| Owner | Single-shard form | Chosen-shard form |
| --- | --- | --- |
| explicit runtime | `Runtime::register_*_service` | `MultiShardRuntime::register_*_service_on` |
| live runtime | `ThreadedRuntime::register_*_service` | `ThreadedMultiShardRuntime::register_*_service_on` |
| simulator | `Simulator::register_*_service` | `MultiShardSimulator::register_*_service_on` |
| canonical live facade | `LocalSystem::register_*_service` | `LocalMultiShardSystem::register_*_service_on` |

Here `*` is `event`, `request`, or `split`. Event ingress uses
`try_send_event(handle, event)` on the owning runtime or facade. The event is
returned as the domain type on explicit-runtime and simulator `Full` or
`Closed` errors; users do not unwrap the internal service envelope.

## Service That Does I/O

The service can answer later.

```text
Request
  -> runtime call
  -> continuation message
  -> maybe another runtime call
  -> final reply
```

This is the right shape for:

- HTTP client
- RPC client
- database client
- persistence service
- service that checks DNS/TLS/process/file state before answering

Do not turn these into spawn-and-route-back helpers just because the answer is
not immediate. Tina can carry the reply context through continuation chains.

## Production-Shaped R&D System

`examples/systems/mini_saas_api` is the current production-shaped R&D system.
It is useful evidence for how the pieces compose, but it is not yet a stable
user template.

It assembles these local-service layers:

| Layer | Skeleton choice |
| --- | --- |
| inbound HTTP | native `tina_http::HttpListener` |
| routing | direct method/path match in the controller isolate |
| domain state | controller isolate fields, not `Arc<Mutex<AppState>>` |
| DB | `tina-sqlite-bridge::SqliteWorker` as the documented one-lane pool shape |
| outbound HTTP | native `tina_http::build_keepalive_pool` |
| readiness | `GET /ready` probes DB and outbound pool state |
| capacity | `GET /debug/capacity` reports body, controller, DB, and outbound surfaces |
| shutdown | mark ingress closed, let admitted work finish or fail visibly, prove readiness reasons, close DB, drain keepalive pool, stop listeners, shutdown runtime |
| replay hook | materialized `live_replay_fact` for the body-cap pressure case |

Route table:

| Route | Turns before reply |
| --- | --- |
| `GET /health` | one |
| `GET /ready` | DB turn, outbound-pool turn |
| `POST /items` | DB insert turn |
| `GET /items/{id}` | DB query turn |
| `POST /items/{id}/notify` | DB query, pool acquire, keepalive request, pool release |
| `GET /debug/capacity` | outbound-pool pressure turn |

Run it:

```sh
cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- smoke
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- pressure
```

This is a skeleton, not a framework. It deliberately keeps route parsing,
small response helpers, and scenario glue specimen-local.

## One HTTP Request Is One Request Tree

A request is a tree. The caller is the root. Every rail the request opens —
a body stream, a timer, a pool wait, a DB or outbound call — is a child.
When the caller goes away, the tree stops waiting.

`tina_runtime::RequestScope` is the bookkeeping for that tree. One service
request owns one scope; the scope holds a clone of each child rail's cancel
handle. A bounded `RequestScopeSet` keyed by request id holds the in-flight
scopes, sized from the budget manifest, not a scattered constant.

```text
request admitted
  -> RequestScope::with_child_cap(id, child_cap)
  -> scope_set.try_insert(req_id, scope)   // Full -> shed, dispatch nothing
  -> defer_scoped(&scope, "db", work).try_admit(...)   // all-or-nothing
  -> ... more scoped children ...
caller goes away (disconnect / timeout / owner stop)
  -> scope.cancel_into_effect(cause, translator)
  -> ScopedRequestReport { cancelled children, capacity reclaimed, ... }
final reply
  -> scope_set.remove(req_id)
```

Admission is all-or-nothing. `defer_scoped(...).try_admit(...)` stores the
pending token, registers the child in the scope, and only then returns the
child effect. If pending or scope admission fails, no child work is
dispatched and the caller authority is handed back so the service answers
deliberately.

Honesty rules the cancel path:

- Scope cancel closes Tina-owned waits and reclaims caller capacity. It does
  not un-start work a bridge already accepted; a late completion still
  becomes a visible rejected trace fact, never a ghost.
- A cross-shard child reports `WrongShard` in its cancel row, never silent
  success.
- A rail Tina cannot cancel (a buffered body already in hand, a
  fire-and-forget send) is recorded as an `UnsupportedScopeRow` in
  `ScopedRequestReport`, not pretended-cancelled.
- Plain `sleep` is not `CallHandle`-cancelable. A request deadline uses a
  `ScopedTimerSet`: cancelling tombstones the ticket, and when the physical
  sleep fires later the continuation observes `ScopedTimerFire::IgnoredLate`
  and skips the user work. The timer is ignored, not magically un-fired.

HTTP rails register through the adapters in `tina_http::scope`:

```rust
// Parked request-body pull, owned by the scope:
let pull = scoped_request_body_pull(&scope, stream.source, "body", t, on_chunk)?;
// A WebSocket send the request owns (the session is not the scope):
let send = scoped_websocket_send(&scope, session, msg, "ws_send", t, on_outcome)?;
// The protocol-honest response-source cancel:
let stop = cancel_response_source(source, t, on_ack);
```

`examples/systems/system_scoped_request_tree` is the small end-to-end proof:
one streaming route, one timer, one cancelable child, one report. A mid-body
disconnect cancels the child, tombstones the timer, and reclaims the slot.
`examples/systems/mini_saas_api` shows the scope on its notify path with the
caps declared as `request.scope_set` / `request.scope_child_cap` budget rows.

## Topology Shapes

The registry should not become a scheduler.

Good registry shape:

```text
service name -> Address<ServiceCall, ServiceReply>
```

Topology lives behind that address.

### Single

One service isolate.

```text
Registry -> SingleService
```

Pressure is the service mailbox capacity and call timeout.

Use for first form and low-concurrency stateful services.

### Pool

One pool isolate owns N worker isolates as resources.

```text
Registry -> WorkerPool -> Worker 0
                       -> Worker 1
                       -> Worker N
```

`tina_runtime::pool::WorkerPool<H, S>` is the bundled shape. Caller
acquires a `PoolLease<H>`, does work against the held handle, then
returns the lease via `Reuse` or `Retire`.

Pressure is:

- pool mailbox capacity (sized `>= max_waiters + burst`)
- `PoolConfig::max_waiters` — caps parked acquirers; surface as
  `AcquireOutcome::Full` once full
- worker mailbox capacities (each worker is a normal isolate)
- caller timeout (the pool does not enforce a separate waiter
  deadline; the caller's `call(...)` timeout is the only one)

Acquires can be cancelled via `cancel_call(handle)` from
`acquire_with_handle_effect`; the pool reclaims the waiter slot on
the next sweep. Drain vs Force close are explicit on
`WorkerPoolMsg::Close(CloseMode::*)`.

Use when calls are independent and can run in parallel and the
caller wants explicit acquire/release lifecycle (DB connection,
worker, AWS client). For "fan one inbound call to one of N workers
without exposing borrow/release" the older `PendingReplies` frontend
pattern is still appropriate; the pool is for explicit borrow.

See [`docs/tina-user-guide/11-ergonomics-checklist.md`](./11-ergonomics-checklist.md#bounded-worker-pool)
for the helpers and `examples/specimen_pool_cancel_reclaim` for a
worked example of the cancel-reclaim flow.

### Sharded

One frontend isolate hashes to N shard-owned services.

```text
Registry -> ShardFrontend -> Shard 0
                          -> Shard 1
                          -> Shard N
```

Pressure is per shard. Hot keys should create visible hot-shard pressure, not a
hidden global queue.

Use when state has a natural key.

`tina_runtime::sharded` ships the small contract surface for this shape:

- `ShardPlacement` — deterministic key-to-shard map over an explicit ordered
  shard list. Visible name, hash scheme, and version. Helper
  `placement.require_owner_str(key, ctx.shard_id()) -> Result<ShardId,
  WrongShard>` folds the canonical owner re-check into one call.
- `ShardServiceTable<M, R>` — typed `ShardId -> Address<M, R>` table built
  over the same shard list. No hidden registry, no `Arc<Mutex<...>>`.
  Build with `ShardServiceTable::from_placement(placement, |shard|
  runtime.register_with_capacity_on(...))` (explicit-step) or
  `try_from_placement(placement, |shard| runtime.register_with_capacity_on(...))`
  for runtimes whose registration returns a `Result`.
- `WrongShard { expected, actual }` — owners re-check the key before
  mutating keyed state and return this typed error on mismatch.
- `ScatterGatherConfig` + `ScatterGatherReport<T>` — bounded fanout knobs
  (max_targets, collector mailbox capacity, per-target timeout, aggregate
  timeout) and a partial-aggregate report (`Replied` / `Full` / `Closed` /
  `Timeout` / `AggregateTimeout` / `MissingShard`). `report.outcomes`
  preserves caller-supplied target order — the report is addressable by
  index, and log output stays deterministic across runs and seeds.
- `ReplyAdapter<M, T, S>` — generic isolate that translates a reply
  message `M` into a coordinator's wider message type `T` (via
  `impl From<M> for T`). Replaces the hand-written bridge isolate every
  scatter/gather coordinator used to need. Register it on a shard with
  explicit mailbox capacity; no hidden queue.
- `ShardBatch<T>` — one shard's grouped items after
  `placement.group_by_owner_str(...)` or `group_by_owner_bytes(...)`.
  The output follows `placement.shards()` order. Empty shards are
  omitted so a 256-shard placement with three items does not produce
  253 empty batches.
- `HotKeyAttemptReport` — caller-owned retry shape. The helpers never retry
  on your behalf; the report distinguishes first-attempt full, retry
  success, retry exhaustion, timeout, and closed.

These are local multi-shard patterns. They are **not** a distributed
database, **not** consensus, **not** remoting, **not** automatic
rebalancing.

A worked example lives in `examples/specimen_sharded_keyspace`: a paired
Tokio (`Vec<Arc<Mutex<HashMap>>>` with hand-rolled FNV-1a placement) and
Tina (`ShardPlacement` + `ShardServiceTable::try_from_placement(...)` +
per-shard `Store` isolates with `placement.require_owner_str(...)`)
implementation that runs the same `SET / GET / DEL / SUM / QUIT` script
and produces the same `Report`. The Tina side uses
`call(addr, msg, timeout).then(continuation)` for keyed access and a
sequential per-shard fanout for `SUM`. The richer parallel
scatter/gather form (with `send_observed` and a `ReplyAdapter`) is
proven in `tina-runtime/tests/sharded_primitives.rs`.

### Grouping items by owner shard

Before fanning out, you may need to group a batch of keyed items so each
shard receives its own sublist:

```rust
let batches = placement.group_by_owner_str(items, |i| &i.key, max_items)?;
for batch in batches {
    let addr = table.address_for(batch.shard)?;
    send(addr, ShardMsg::Batch(batch.items));
}
```

`group_by_owner_str` (and `group_by_owner_bytes`) returns `Vec<ShardBatch<T>>`
in `placement.shards()` order. Empty shards are omitted. The input count is
capped: too many items returns `GroupByOwnerError::CapExceeded`. The helper
uses the same `owner_for_bytes` path live and simulated runtimes share, so the
grouping is byte-identical across both.

### Hot keys and caller-owned retry

A hot key hashes to one owner shard. When that shard's mailbox is full, Tina
reports `Full` on that shard. It does not smear the pressure across other
shards or hide it in a global queue. The caller sees the hot-shard truth
through `HotKeyAttemptReport`:

```rust
let mut report = HotKeyAttemptReport::default();
match runtime.try_send(owner, msg) {
    Ok(()) => report.record(HotKeyAttemptOutcome::Accepted),
    Err(TrySendError::Full(_)) => report.record(HotKeyAttemptOutcome::FullFirstAttempt),
    Err(TrySendError::Closed(_)) => report.record(HotKeyAttemptOutcome::Closed),
}
```

If the caller retries, the retry loop is caller-owned. The runtime never
retries on your behalf because the caller owns idempotency: a retry may mean
"send the same message again", and only the caller knows whether that is safe.
`HotKeyAttemptReport` records first-attempt `Full` separately from retry `Full`,
`RetrySucceeded`, and `RetryExhausted` so the retry pressure stays visible in
logs and metrics.

## Deferred Replies

`call(svc, msg, timeout).then(...)` works when the service answers in
one handler turn. Some shapes need to answer later:

- pool frontend, reply arrives from one of N workers
- sharded frontend, reply arrives from key owner
- bridge worker, many external requests in flight
- fanout, aggregate after N partial results

Capture the caller, store it, answer later. The short way is `park`:

```rust
// Split-service request handler:
match self.pending.park_request(req_id, call) {
    Ok(_ticket) => start_work(req_id),
    Err(ParkError::Full { call, .. }) => call.reply(MyReply::Full),
    Err(ParkError::DuplicateKey { call, .. }) => call.reject(...),
}

// Plain handle_call:
match self.pending.park_call(req_id, call) {
    Ok(_ticket) => start_work(req_id),
    Err(ParkCallError::Full { call, .. }) => call.reply(MyReply::Full),
    Err(_) => unreachable!("monotonic key + local-only call"),
}

// Settle later:
return self.pending.reply_ticket::<Self>(ticket, MyReply::Ok(value));
```

`park_request` / `park_call` check duplicate and capacity *before*
consuming caller authority, so a `Full` or `DuplicateKey` rejection
returns the original `RequestCall` / `CallContext` for an immediate
typed reply. `ParkTicket` is move-only with private fields; user code
cannot forge or duplicate one, and a stale ticket against a reused slot
is rejected as `TakeParkedError::StaleTicket`.

The lower-level key-only form still works as an escape hatch:

```rust
let slot: DeferredReply<MyReply> = ctx.take_request_context()?.into_deferred();
self.pending.try_insert(req_id, slot)?;
let slot = self.pending.take(&req_id).expect("slot for id");
return reply_to(slot, MyReply::Ok(value));
```

One-shot. After `take_request_context`, a stray `Effect::Reply` in the same
turn is a no-op for that caller.

### Pending box needs a cap

```rust
PendingReplies::<RequestId, MyReply>::with_capacity(64)
```

Sweeps abandoned slots before each admission, so timed-out callers do
not eat capacity. `try_insert` returns `Full` when no slot can be
reclaimed and `DuplicateKey` when the id is already live.

> Mailbox holds messages. Pending box holds promises. Both need caps.

### Two caps, not one

- **Mailbox** bounds incoming messages.
- **Pending box** bounds captured callers.

Roomy mailbox + tiny pending box = accepts work, holds few callers.
The inverse rejects early, holds many. Pick both.

### Trace facts

Every captured slot ends with one of:

- `DeferredReplyCaptured` — slot taken.
- `DeferredReplySent` — caller got the reply.
- `DeferredReplyRejected { reason: CallerClosed | ReplyPathFull |
   RequesterShardClosed | TypeMismatch }` — caller gone, reply path
  failed, or the reply payload type didn't match the dispatching
  `Address<_, R>`.
- `DeferredReplyDropped` — service let the slot drop while caller was
  still open.

Duplicate replies and post-drop replies are prevented by the type
system: `reply_to` consumes the `DeferredReply` handle.

### Anti-pattern

Don't roll your own:

```rust
struct Bad {
    pending: Arc<Mutex<HashMap<RequestId, OneShot<MyReply>>>>,
}
```

No cap, no caller-liveness signal, no terminal trace, no sweep. Use
`DeferredReply` + `PendingReplies`.

### Request Context

When the reply is intentionally multi-turn, use `RequestContext<R>` instead of
`DeferredReply`. The name signals intent to readers.

```rust
call_ctx
    .defer(call(probe, ProbeMsg, timeout))
    .reply(MyMsg::ProbeResult)
```

The continuation message still carries `RequestContext<MyReply>`, so the
message enum stays honest:

```rust
ProbeResult(RequestContext<MyReply>, CallOutcome<ProbeReply>)
```

The caller timeout still governs. The service still answers later with
`reply_to(req, MyReply)`. There is no hidden state preservation and no
async-looking sugar.

When the expanded authority move reads better, spell it out:

```rust
let req: RequestContext<MyReply> = call_ctx.into_request_context();
call(probe, ProbeMsg, timeout)
    .then_with_request(req, MyMsg::ProbeResult)
```

Use `then(...)` for ordinary continuations that do not carry caller authority.
Do not use ordinary `then(...)` as the default in `handle_call` unless you also
reply, reject, or defer the `CallContext`.

`RequestContext` is a real newtype over `DeferredReply`; it exists so that
a handler signature can say "I carry the promise across turns" instead of
"I hold an opaque slot." Use whichever spelling makes the code clearer.

## Host Call From Tests And Setup

Tests, specimens, and setup code often need to drive one service call
from the host thread and read the result. Both threaded runtimes expose
`call_blocking` for exactly this:

```rust
// single-shard
let outcome = runtime.call_blocking(addr, MyMsg::Probe, Duration::from_secs(2));

// multi-shard, routed by the address's owning shard
let outcome = multi_runtime.call_blocking(shard_addr, MyMsg::Probe, Duration::from_secs(2));
```

Both forms:

- register a one-shot driver isolate behind a **bounded** worker
  command admission — a full queue surfaces as
  `ThreadedRuntimeError::CommandFull`, not as a silent host hang;
- preserve the normal Tina call outcomes: `Replied`, `Full`,
  `Closed`, `Timeout`, `Rejected`;
- do **not** cancel accepted work when the host wait ends.

The multi-shard form panics on an unknown shard, matching `try_send`
and `observe_result`. There is no `call_blocking_on` in this phase;
the routing-by-address-shard shape removes one place callers can get
the shard id wrong.

**Do not call `call_blocking` from inside an isolate handler.**
Handlers must stay synchronous and non-blocking. Use
`call(...).then(...)` inside isolates.

Copied sharded smoke shape:

```rust
let runtime = ThreadedMultiShardRuntime::new(shards, DefaultThreadedMailboxFactory);
let addr = runtime.register_with_capacity_on::<MyService, _>(shard_id, svc, cap)?;
runtime.try_send(addr, MyMsg::Bootstrap)?;
let outcome = runtime.call_blocking(addr, MyMsg::Ping, Duration::from_secs(2))?;
```

See `examples/systems/system_session_auth` for a real sharded
specimen using `ThreadedMultiShardRuntime::call_blocking`.

## Admission Policy

For edge services that need explicit shed/rate-limit/per-tenant caps on
top of the mailbox, see
[`06-boundedness-and-overload.md`](./06-boundedness-and-overload.md#admission-and-rate-policy).
The vocabulary is small and composes with the patterns below — every
admission decision returns a typed outcome the caller matches and a
move-only proof when admission succeeded.

## Park, Wait, Guard, Cancel

Four shapes show up in nearly every multi-turn service. Each is named
by the user's job, then by the helper that owns it. Copied paths start
from the user-intent name. Lower-level names stay public for code that
already reads better under the mechanism name.

### Many callers wait for one result → `SharedWork`

> *Several callers asked for the same key. The service starts the
> upstream work once and replies every parked caller with one value.*

```rust
let mut shared: SharedWork<CacheKey, CacheReply> =
    SharedWork::with_capacity(64).named("cache.shared");

// On Get(key) from handle_request:
match shared.wait(key.clone(), call) {
    Ok(ticket) => {
        if entry.filling.is_some() {
            return request_effect_after_shared_wait(&ticket, noop());
        }
        let fill = sleep(self.fill_delay).then(move |result| {
            ServiceMessage::Event(CacheEvent::FillDone { key, result })
        });
        entry.filling = Some(FillState::default());
        request_effect_after_shared_wait(&ticket, fill)
    }
    Err(SharedWorkError::Full { call, .. })
    | Err(SharedWorkError::KeyFull { call, .. }) => call.reply(CacheReply::Busy),
}
```

- What the user is doing: coalesce many callers behind one upstream fill.
- Helper to use: `SharedWork::wait` (or `wait_call` for `CallContext`).
- What stays explicit: the fill-in-flight flag, stale fill generation,
  the upstream call/timer, retry policy.
- What not to use: hand-rolled `HashMap<key, VecDeque<id>>` next to
  `PendingReplies`. That is exactly what `SharedWork` exists to replace.

### One active cancelable request per key → `PendingCancelableCallSet`

> *This key has at most one in-flight request at a time. A second attempt
> with the same key is rejected immediately so the caller can answer
> `Busy` or `AlreadyRunning`.*

```rust
let mut pending: PendingCancelableCallSet<JobId, Q, R> =
    PendingCancelableCallSet::with_capacity(64);

match pending.try_insert(token) {
    Ok(ticket) => admit_effect,
    Err(PendingCancelableInsertError::Full { token })
    | Err(PendingCancelableInsertError::DuplicateKey { token }) => {
        let request = token.into_request_context();
        reply_to(request, JobReply::Busy)
    }
}
```

- What the user is doing: enforce "one job per id" with caller-owned
  cancel.
- Helper to use: `PendingCancelableCallSet::try_insert`.
- What stays explicit: dispatching the child effect, classifying
  worker-return outcomes, cancel translation.
- What not to use: storing pending tokens in a plain `HashMap`. That
  loses the `Full` / `DuplicateKey` distinction and the move-only
  ticket.

### Many cancelable requests grouped by key → `CancelableWork`

> *This key can have several in-flight attempts at once — retry attempts,
> concurrent racers, multi-tenant per-key fanout. Every attempt gets its
> own move-only `WorkTicket` so a stale completion cannot remove a newer
> admit that reused the key.*

```rust
let mut work: CancelableWork<JobId, Q, R> =
    CancelableWork::with_key_limit(64, 4).named("job.attempts");

match work.admit(token) {
    Ok((ticket, request_effect)) => request_effect,
    Err(AdmitWorkError::Full { token }) | Err(AdmitWorkError::KeyFull { token }) => {
        let request = token.into_request_context();
        reply_to(request, JobReply::Busy)
    }
}
```

- What the user is doing: allow more than one live attempt per natural
  key, with caller-owned cancellation per attempt.
- Helper to use: `CancelableWork::admit` (returns a `WorkTicket`).
- What stays explicit: dispatch of the child effect, completion
  removal by ticket, drain on stop.
- What not to use: `PendingCancelableCallSet` when more than one live
  attempt per key is allowed; `Full` becomes the wrong story.

### One caller waits for one key → `PendingReplies`

> *Reply slots are owned by id, unrelated to each other. No coalescing,
> no per-key cap.*

`PendingReplies::park_request` / `park_call` returns a `ParkTicket`.
Use this when slots are independent — the table does not know that two
ids both refer to the "same cache key."

### One caller, one key, plus an RAII guard → `GuardedPendingReplies`

`GuardedPendingReplies::park_request_guarded` pairs a parked caller with
one RAII guard. The guard drops exactly once on reply, drain, or
caller-gone sweep.

When the guard is a `SharedCapacityReservation`, this is the copied
path for multi-turn work that holds shared capacity:

```rust
let reservation = match SharedCapacityReservation::try_reserve([
    in_flight.charge(1),
    body_bytes.charge(request_len),
]) {
    Ok(reservation) => reservation,
    Err(full) => return call.reply(Reply::Full(full.scope)),
};

match pending.park_call_guarded(id, call, reservation) {
    Ok(_ticket) => sleep(work).then(move |result| Msg::Done { id, result }),
    Err(error) => error.into_call().reply(Reply::Busy),
}
```

The service stores one thing: the pending reply slot plus the guard.
There is no parallel "remember to release the budget" table.

### First success across calls → `CallGroup::start_cancelable`

For "race these calls, answer with the first good result, cancel the
rest," use `CallGroup::start_cancelable`.

```rust
let mut group = CallGroup::with_capacity(providers.len());
let mut effects = Vec::new();

for (key, provider) in providers.iter().copied().enumerate() {
    let effect = group.start_cancelable(
        key,
        call_cancelable(provider, ProviderMsg::Quote, timeout),
        |key, token, outcome| Msg::ProviderReturned { key, token, outcome },
    )?;
    effects.push(effect);
}
```

The helper reserves the token, stores the cancel handle, and returns
the effect only after the group can track it. That removes the old
"reserve token, build effect, insert handle" dance. The continuation
still carries the token back, so stale completions cannot remove a
newer branch that reused the same key.

### Reply later to the current caller → `call.defer(...).reply(...)`

> *The handler has caller authority right now but needs to do some work
> first. Capture the request, return an effect that ends with replying
> to that captured request.*

```rust
call.defer(sleep(delay)).reply(move |request, result| {
    Msg::Finished { request, result }
})
```

The continuation arrives at `handle`, not `handle_call`. The original
caller receives the final reply.

### Close / drain on stop

`SharedWork::drain_all_with(factory)`, `CancelableWork::drain(factory)`,
and `PendingReplies::drain(...)` reply every open caller with a
terminal value before the service stops. No silent drop. See
[14-lifecycle-and-shutdown.md](14-lifecycle-and-shutdown.md) for the
broader resource-close story.

### Write a bridge

For the bridge-author copy path (install, close, drain, metrics,
pressure, classifier, late-result truth), see
[30-bridge-author-kit.md](30-bridge-author-kit.md).

### Capacity surface

All four helpers expose the same surface: `capacity / len / high_water /
full_rejects / capacity_report().named(...)`.

## Timer Continuation

For "wake me later with this event", use `then_event`:

```rust
sleep(delay).then_event(move || Msg::Wake { id })
```

`then_event` lives only on the value returned by `tina_runtime::sleep`.
A non-timer `TypedCall<()>` (TCP close, file ops, signal wait) keeps
returning `Result<(), CallError>` and must be consumed with `.then(...)`
so the error path stays visible.

## Macro Rule

A future `#[service]` macro may hide byte encoding.

It may not hide backpressure.

Generated service code must still make these visible:

- mailbox capacity
- full
- closed
- timeout
- decode error
- unknown method
- internal error

Convenience may remove ceremony. Convenience must not remove truth.
