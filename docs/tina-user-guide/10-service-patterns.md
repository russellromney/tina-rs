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

One frontend isolate owns N worker isolates.

```text
Registry -> PoolFrontend -> Worker 0
                         -> Worker 1
                         -> Worker N
```

Pressure is:

- frontend mailbox capacity
- worker mailbox capacities
- max in-flight routing state
- caller timeout

Use when calls are independent and can run in parallel.

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
- `HotKeyAttemptReport` — caller-owned retry shape. The helpers never retry
  on your behalf; the report distinguishes first-attempt full, retry
  success, retry exhaustion, timeout, and closed.

These are local multi-shard patterns. They are **not** a distributed
database, **not** consensus, **not** remoting, **not** automatic
rebalancing.

A worked example lives in `examples/eiffel_sharded_keyspace`: a paired
Tokio (`Vec<Arc<Mutex<HashMap>>>` with hand-rolled FNV-1a placement) and
Tina (`ShardPlacement` + `ShardServiceTable::try_from_placement(...)` +
per-shard `Store` isolates with `placement.require_owner_str(...)`)
implementation that runs the same `SET / GET / DEL / SUM / QUIT` script
and produces the same `Report`. The Tina side uses
`call(addr, msg, timeout).reply(continuation)` for keyed access and a
sequential per-shard fanout for `SUM`. The richer parallel
scatter/gather form (with `send_observed` and a `ReplyAdapter`) is
proven in `tina-runtime/tests/sharded_primitives.rs`.

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
