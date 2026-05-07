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
