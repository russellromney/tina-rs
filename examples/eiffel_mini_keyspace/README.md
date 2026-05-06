# Eiffel Mini Keyspace

Paired Tokio-vs-Tina implementation of a tiny Redis-shaped key/value service.

This is an ergonomics and feature comparison, not a load test. The protocol is
line-oriented and intentionally tiny:

```text
SET key value
GET key
DEL key
QUIT
```

Run both sides:

```bash
cargo run --manifest-path examples/eiffel_mini_keyspace/Cargo.toml -- compare
```

Run one side:

```bash
cargo run --manifest-path examples/eiffel_mini_keyspace/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_mini_keyspace/Cargo.toml -- tina
```

Both sides emit the same response and produce identical
`SideReport { ok=1, values=1, misses=2, deleted=1 }` numbers; the runs are
asserted in `print_report` to keep the comparison honest.

## What this comparison taught us

### Tokio side

- Trivial. ~50 lines: bind, accept, `read_to_end`, run a `BTreeMap`, write the
  response. Sequential async/await with one `BTreeMap` owned by the task is the
  most ergonomic shape Rust offers for this problem.
- State ownership is implicit. The map lives on the stack of the connection
  task. There is no story for "what isolate owns this state" — there is just
  the task that happens to be running.
- Easy to write, easy to misuse: it would be just as easy to wrap the map in
  `Arc<Mutex<_>>` and share it across handlers, which is exactly the kind of
  shared-mutable trap Tina is built to prevent.

### Tina side

What worked well:

- The `Store` isolate genuinely owns the `BTreeMap`. There is no possible way
  for another isolate to touch it. That is the property we want, and the
  `#[isolate(message = …, reply = …)]` macro made declaring it cheap.
- `call(addr, msg, timeout).reply(map_outcome)` is a clean way to express
  request/reply at the boundary between the connection state machine and the
  store. The continuation is just another message variant.
- The `Connection` isolate as an explicit state machine
  (`Begin → Read → (StoreReturned)* → Wrote → Closed`) reads naturally once
  written. Each transition is one match arm and one effect.

What was awkward or surprising:

- Driving "process the next command" required a hand-rolled `next_effect()`
  helper that pops a command off `VecDeque<Command>` and either issues another
  `call(...)` or transitions to `tcp_write`. There is no built-in "loop" or
  "for each" effect; the recursion through self-sent messages or helper
  methods is the only option, and it is verbose.
- `CallOutcome<StoreReply>` has to be unwrapped via `into_result()` and matched
  on every variant. For a store that always replies, the timeout/cancel arms
  are effectively dead code, but you still have to write them.
- Capacities matter and are easy to get wrong. The connection mailbox needs
  enough room for the inbound `StoreReturned` callbacks plus the trailing
  `Wrote`/`Closed` messages; we picked 16 and moved on, but a smaller number
  silently breaks the run. There is no obvious "right" default for this.
- A custom `Mailbox` + `MailboxFactory` pair is required just to instantiate
  `ThreadedRuntime`. The `tina-mailbox-spsc` crate exists for tests, but
  examples still copy ~40 lines of `Rc<RefCell<VecDeque<_>>>` boilerplate. A
  default in-process mailbox would remove the friction.
- The bound listener address has to be smuggled out through an
  `Arc<Mutex<Option<SocketAddr>>>` because nothing in the runtime exposes
  "tell the outside world what port you got". For real TCP examples this is a
  recurring papercut — every comparison so far has reinvented `BoundAddr`.
- Shutdown still relies on `complete_trace()` polling for a specific
  `CallKind::TcpStreamClose` event. Useful for tests, but not a story we want
  to ship. The runtime needs a "this isolate finished cleanly" signal that
  external code can await without scanning the trace.
- The `#[isolate(... shard = KeyspaceShard)]` attribute requires every
  isolate to declare a shard even when there is only one. Single-shard
  examples should be allowed to omit it.

### Suggested follow-ups for Tina (recorded for the roadmap)

- Provide a default `MailboxFactory` for in-process examples.
- Provide a "wait for isolate to stop" handle so tests/examples don't
  scrape the trace.
- Consider sugar for "issue a sequence of calls then write a buffer" — the
  `next_effect()` shape will recur in nearly every connection-handling
  isolate.
- Consider returning the bound `SocketAddr` as part of `tcp_bind`'s reply
  in a way the spawning code can read without a side-channel mutex.
