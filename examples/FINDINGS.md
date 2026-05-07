# Eiffel Findings

This file is the current action list from Eiffel.

Eiffel examples are specimens: they show how Tokio and Tina code feel for the
same kind of job. When the same Tina pain appears across specimens, it becomes
runtime/API work here.

Resolved history and the longer field journal live in
[`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md). Per-example notes stay in each
example's own `README.md`.

## Product Improvements

### 1. Typed isolate result waiters

**Surfaced by:** `eiffel_mux_client`, `eiffel_persistent_counter`,
`eiffel_outbound_fetch`, `eiffel_outbound_http`,
`eiffel_graceful_shutdown`.

Host/test code still reaches for `Arc<Atomic*>`, `Arc<Mutex<_>>`, per-op
correlators, or tiny `Driver` isolates when it wants final app data from an
isolate. Tina can already observe that an isolate stopped; the missing piece is
the typed value the isolate stopped with.

**Build:** a bounded `IsolateResultWaiter<T>` / typed join-handle shape:

```rust
let done = runtime.observe_result(addr);
let result: T = done.wait(timeout)?;
```

This should preserve Tina truth: capacity explicit, timeout explicit, stopped /
closed / dropped outcome typed, trace still says the isolate stopped.

### 2. Continuation and pipeline sugar

**Surfaced by:** `eiffel_mini_keyspace`, `eiffel_mux_client`,
`eiffel_persistent_counter`, `eiffel_outbound_fetch`,
`eiffel_graceful_shutdown`, `eiffel_outbound_http`.

The honest Tina shape is good: every runtime call returns as a message. But
linear protocols still grow enum variants quickly:
`Begin -> Connected -> Wrote -> Read -> Closed`.

**Build:** small Tina-shaped sugar for linear chains and "process this list one
item at a time" flows. This must not become fake async/await and must not hide
timeouts, `Full`, `Closed`, or trace events.

Good direction:

- continuation aliases or generated continuation names;
- a bounded "for each command, call service, accumulate, continue" helper;
- clearer `sequence(...)` examples for request/reply chains.

### 3. First-class TCP loop helpers

**Surfaced by:** `eiffel_outbound_fetch`, `eiffel_mux_client`, HTTP/RPC work.

Every real TCP protocol wants write-all, read-exact, read-to-EOF, and framed
read loops. Today each specimen writes the loop by hand.

**Build:** Tina runtime-call helpers or reusable state-machine helpers:

```rust
tcp_write_all(stream, bytes).reply(...)
tcp_read_exact(stream, len).reply(...)
tcp_read_to_eof(stream, max).reply(...)
```

Important constraint: helpers may remove ceremony, but partial progress must
stay trace-visible. No hidden buffer growth, no hidden retries.

### 4. Capacity diagnostics and reply-slot budgets

**Surfaced by:** `eiffel_real_io_chat`, `eiffel_mini_keyspace`.

Mailbox capacity is "incoming messages + replies to my outstanding work." That
is correct, but users experience it as magic numbers.

**Build:**

- better runtime diagnostics for reply rejected because requester mailbox full;
- role-based capacity presets or budget helpers;
- maybe separate `incoming` vs `reply` capacity at registration if the model
  stays clean;
- examples that show how to size listener/session/service mailboxes.

Tina should make boundedness obvious, not mystical.

### 5. Tiny native HTTP router

**Surfaced by:** `eiffel_native_http`.

Native HTTP is real now. The remaining boring gap is route matching:
Tina handlers currently match `(method, path)` by hand.

**Build:** a small Tina HTTP routing helper, not a web framework:

```rust
HttpRouter::new()
    .get("/counter", ...)
    .post("/counter", ...)
```

or an isolate-friendly mapping that emits user messages. Keep service state in
isolates. Do not recreate Axum/Tower inside Tina.

### 6. Bridge specimen cleanup

**Surfaced by:** `eiffel_axum_counter`, `eiffel_ws_room`.

The bridge path is still important for adoption, but the remaining bridge
examples are intentionally deferred until bridge ergonomics settle. They still
teach useful things, but they have the old `src/comparison/` shape.

**Build:**

- rewrite bridge examples under the specimen rule after the 058 bridge surface
  is the blessed one;
- show `BridgeHost` lifecycle clearly;
- show `BridgeClient::call` / async RPC bridge where relevant;
- document signal-handler coexistence when Tokio and Tina live in one process.

### 7. RPC service topology beyond single

**Surfaced by:** `eiffel_rpc`, phase 052/058.

Typed RPC services now feel much better, but `SingleService` is only the first
topology. Hot services need a way to become a pool or sharded set without the
registry API changing.

**Build:**

- real `PooledService`;
- real `ShardedService`;
- explicit mailbox/capacity semantics for each;
- docs that say how `Full`, `Closed`, `Timeout`, and partial failure behave.

The registry should keep mapping service name to one address. The address may
be a single service, pool frontend, or shard router.

### 8. Uniform overload reports for pressure runners

**Surfaced by:** `eiffel_cpu_run`, `eiffel_mem_run`.

The wrapper runners can say "the specimen still completed under pressure."
They cannot yet compare accepted/full/closed/timeouts across specimens in one
small vocabulary.

**Build:** a lightweight report convention for pressure-capable specimens:

```text
accepted full closed timeouts other rss_peak exit
```

Keep it local and boring. Do not reintroduce a shared harness that constrains
how examples are written.

## Resolved Or Retired By Recent Phases

These used to be current pain and should not be copied into new code:

- hand-rolled mailbox factories: use `DefaultMailboxFactory` /
  `DefaultThreadedMailboxFactory`;
- `Arc<Mutex<Option<SocketAddr>>>` for listener bind address: use
  `observe_next_bound()`;
- trace fingerprinting via `Debug`: use `RuntimeEvent::stable_hash()` /
  `stable_trace_hash(...)`;
- one-off shard types for single-shard programs: use `SingleShard` or omit
  `shard = ...`;
- `Arc::try_unwrap` bridge shutdown dances: use the bridge host lifecycle;
- old shared comparison harnesses: examples are specimens, tests are proof.

## How To Add A Finding

Only add to this file when the finding implies Tina product work.

Use:

```md
### N. Short product-shaped title

**Surfaced by:** `example_name`, `other_example`.

What repeated pain we saw.

**Build:** concrete primitive, API, doc, or test work.
```

Per-example flavor belongs in the example README. Resolved archaeology belongs
in `FINDINGS_HISTORY.md`.
