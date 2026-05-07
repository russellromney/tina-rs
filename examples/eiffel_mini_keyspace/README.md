# eiffel_mini_keyspace

Same scripted Redis-shaped workload, two implementations, real
loopback TCP:

```text
SET llama hay
GET llama
GET missing
DEL llama
GET llama
QUIT
```

The wire protocol is line-oriented and tiny on purpose. The
comparison is about how each runtime owns the `BTreeMap`, not about
performance.

## Run

```sh
cargo run --manifest-path examples/eiffel_mini_keyspace/Cargo.toml -- both
cargo run --manifest-path examples/eiffel_mini_keyspace/Cargo.toml -- tokio
cargo run --manifest-path examples/eiffel_mini_keyspace/Cargo.toml -- tina
```

You'll see the same counts on both sides:

```
comparison=eiffel_mini_keyspace side=tokio ok=1 values=1 misses=2 deleted=1
comparison=eiffel_mini_keyspace side=tina  ok=1 values=1 misses=2 deleted=1
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

Both files are self-contained — server, client, classification.

## Tokio shape

One async block. `BTreeMap<String, String>` lives on the stack of the
connection task. `read_to_end`, `for command in parse_commands(...)`,
`match command`, `write_all`. Sequential, ergonomic, and structurally
indistinguishable from "what someone would write at 10pm." There is
no story for "what owns this state" — there's just the task.

When you run it: every command executes, the response goes out, the
counts come back exactly as scripted.

## Tina shape

Three isolates, each owns one thing:

- **`Store`** — owns the `BTreeMap`. Only way in or out is via
  `StoreMsg` / `StoreReply`. Nothing else can read or change it.
- **`Connection`** — owns the accepted TCP stream and a queue of
  parsed commands. Walks `Begin → Read → (StoreReturned)* → Wrote →
  Closed`. Each command is one `call(self.store, ..., timeout)`.
- **`Listener`** — `tcp_bind` → `tcp_accept` → `spawn(Connection)`.

`call(addr, msg, timeout).reply(continuation)` is how each command
crosses into the store and back. The continuation is just another
message variant.

When you run it: identical wire output and identical counts. Same
script, different shape underneath.

## Discussion

What feels better:

- **Owned state by construction.** `Store` is the only thing that
  can read or change `values`. There is no syntactic path to
  `Arc<Mutex<_>>`. The Tokio version could grow that wart any time
  someone needs to share the map with another task; the Tina version
  cannot.
- **Request/reply at a boundary reads honestly.** `call(addr, msg,
  timeout).reply(map_outcome)` is verbose vs `await`, but it's also
  one message in, one message out, no hidden state machine, no
  implicit cancellation point.
- **The connection state machine is one match.** `Begin → Read →
  StoreReturned → Wrote → Closed` is one arm per transition. Once
  written, it's the easier-to-trace piece in the whole example.

What feels worse:

- **Three isolates for a single connection.** Tokio is one async
  block. Tina has Store + Connection + Listener. Each piece is small
  (047 retired the mailbox-factory, per-shard-type, and bound-address
  side channels) but there are more of them.
- **"Process the next command" is hand-rolled.** The Connection's
  `next_effect()` helper pops a command off the queue and tail-calls
  itself through self-sent messages. There is no built-in iteration
  combinator for "for each command, do `call(...)` then continue."
  This shape is going to recur in every connection handler.
- **`CallOutcome<StoreReply>` carries `Timeout` / `Closed` arms that
  effectively never fire.** For a store that always replies, the
  failure arms are dead code, but the type forces every call site to
  match them. `FINDINGS.md` tracks the broader continuation/pipeline
  sugar work.

What this suggests:

- The owned-state-via-isolates pattern is the right default for any
  long-lived service state. The `Store` isolate is the cleanest piece
  in this comparison.
- The "process a list of things" recursion (`next_effect`) is the
  next ergonomics frontier. A combinator that expressed "for each
  command, call the store, accumulate the reply" would shrink every
  connection-handling isolate.
- The runtime knows when in-process calls cannot time out (the
  callee doesn't reply on a timer). Surfacing that as a narrower
  outcome type would retire a lot of the dead-code matches in
  connection state machines.
