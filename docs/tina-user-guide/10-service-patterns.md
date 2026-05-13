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
    fn handle(&mut self, msg: CounterMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            CounterMsg::Add(n) => {
                self.value += n;
                reply(CounterReply::Value(self.value))
            }
        }
    }
}
```

Caller:

```rust
call(counter, CounterMsg::Add(1), Duration::from_millis(20))
    .reply(ClientMsg::CounterReturned)
```

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
`call(addr, msg, timeout).reply(continuation)` for keyed access and a
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

`call(svc, msg, timeout).reply(...)` works when the service answers in
one handler turn. Some shapes need to answer later:

- pool frontend, reply arrives from one of N workers
- sharded frontend, reply arrives from key owner
- bridge worker, many external requests in flight
- fanout, aggregate after N partial results

Capture the caller as a typed `DeferredReply<R>`, store it, answer
later:

```rust
let slot: DeferredReply<MyReply> = ctx.take_reply_slot()?;
self.pending.try_insert(req_id, slot)?;
// later turn:
let slot = self.pending.take(&req_id).expect("slot for id");
return reply_to(slot, MyReply::Ok(value));
```

One-shot. After `take_reply_slot`, a stray `Effect::Reply` in the same
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
let req: RequestContext<MyReply> = call_ctx.into_request_context();
call(probe, ProbeMsg, timeout)
    .reply_with_request(req, MyMsg::ProbeResult)
```

`reply_with_request` boxes a translator that carries the context into the
continuation message. The caller timeout still governs. The service still
answers later with `reply_to_request(req, MyReply)`. There is no hidden
state preservation and no async-looking sugar.

`RequestContext` is a real newtype over `DeferredReply`; it exists so that
a handler signature can say "I carry the promise across turns" instead of
"I hold an opaque slot." Use whichever spelling makes the code clearer.

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
