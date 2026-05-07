# Eiffel Findings

This file is the current action list from Eiffel.

Eiffel examples are specimens: they show how Tokio and Tina code feel for the
same kind of job. When the same Tina pain appears across specimens, it becomes
runtime/API work here.

Resolved history and the longer field journal live in
[`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md). Per-example notes stay in each
example's own `README.md`.

## Product Improvements

### 1. Typed isolate result waiters — landed in 059 Rock 1

Closed by Phase 059 Rock 1. Use:

```rust
// isolate
stop_with(self.outcome.clone())

// host
let result = runtime.observe_result::<T, _, _>(addr)?;
let value = result.wait(timeout)?;
```

Bounded one-slot, single-claim per `(isolate, generation)`, no replay
cache. Eager `AlreadyStopped` / `AlreadyClaimed` / `ObservationFull` at
register time; `Timeout` / `RuntimeStopped` / `StoppedWithoutResult` /
`TypeMismatch` at `wait`. Trace still emits `IsolateStopped`; the new
`EffectKind::StopWith` distinguishes the with-result path.

Already converted: `eiffel_outbound_fetch` (3 atomics removed),
`eiffel_mux_client` (`Arc<Mutex<Vec<u32>>>` removed).
`eiffel_persistent_counter`, `eiffel_outbound_http`, and
`eiffel_graceful_shutdown` use a per-op correlator pattern that is
*not* a "final value after stop" — those need a separate ergonomics
pass and are not 059 Rock 1 work.

### 2. Continuation and pipeline sugar — landed (first form) in 059 Rock 2

Closed by Phase 059 Rock 2 as "documented canonical pattern + reply
aliases" rather than a macro. `tina_runtime` ships per-call-kind
reply aliases (`TcpConnectReply`, `JournalAppendReply`,
`SignalWaitReply`, `FileReadReply`, …) so isolate enums spell the
call kind by name instead of `Result<X, CallError>`. Chapter 16
("Continuation And Pipeline Patterns") in the user guide is the
blessed shape for pipeline + list-processing isolates and names the
four anti-patterns (hidden retry, multi-call effects on one
resource, async wrapper, shared accumulator).

Already converted: `eiffel_outbound_fetch`, `eiffel_persistent_counter`,
`eiffel_mux_client`, `eiffel_graceful_shutdown`, `eiffel_mini_keyspace`,
`eiffel_real_io_chat`, `eiffel_rpc`.

Deliberately not shipped: a `pipeline!` macro, a `for_each` helper,
or anything that would hide per-step trace truth.

### 3. First-class TCP loop helpers — landed (client-side first form) in 059 Rock 3

Closed by Phase 059 Rock 3. `tina_runtime::tcp_loops` ships:

- `TcpWriteAll` — partial-write loop;
- `TcpReadExact` — partial-read loop with `EarlyEof(partial)` outcome;
- `TcpReadToEof` — read until empty bytes or a `max` byte cap.

Each helper is a small client-side state machine. Each
`next_effect`/`advance` step expands to exactly one `tcp_write` /
`tcp_read`, so partial progress is one trace event per call.

Deferred to a future phase: turning these into runtime-owned
`CallInput::TcpWriteAll` / `TcpReadExact` / `TcpReadToEof` so the
isolate dispatches one effect rather than maintaining helper state.
That's a substrate change (betelgeuse + tina-sim + driver) and Rock 3
intentionally shipped the smaller form first. `eiffel_outbound_fetch`
already uses the helpers; framed-read still hand-rolls (see Rock 2's
"continuation pattern").

### 4. Capacity diagnostics and reply-slot budgets — landed in 059 Rock 4

Closed by Phase 059 Rock 4. `tina_runtime` ships:

- `PressureSummary::from_events(events)` — counted summary of every
  pressure-shaped trace event (mailbox-full, reply-path-full,
  send-full, lifecycle-closed);
- `Runtime::pressure_summary()` / `ThreadedRuntime::pressure_summary()`
  accessors;
- `MailboxBudget { incoming, replies }` with `.total()` plus
  `listener` / `session` / `service` / `fanout` presets that name
  the arithmetic at the spawn site.

Chapter 6 ("Boundedness And Overload") rewritten to walk the
`total = incoming + replies` math and show the diagnostics API.

### 5. Bounded host send helpers — landed in 059 Rock 5

Closed by Phase 059 Rock 5 (commit 4a9df12). `ThreadedRuntime::send_blocking`
/ `send_retrying` are shipped with the contract above intact: bounded wait,
caller-visible timeout, typed `Sent`/`Full`/`Timeout`/`Closed`/`WorkerStopped`
outcomes, no hidden queue.

### 6. Tiny native HTTP router — landed in 059 Rock 6

Closed by Phase 059 Rock 6. `tina_http` ships:

- `Router` — stateless `fn(&HttpRequest) -> HttpResponse` handlers;
- `StatefulRouter<S>` — handlers with `&mut S` access for the
  in-isolate case where routes mutate state;
- both expose `.get`/`.post`/`.put`/`.delete`/`.patch` sugar over
  the generic `.route(method, path, handler)`;
- opt-in `.method_not_allowed()` distinguishes 405 (path known,
  method mismatch) from 404 (path unknown).

Already converted: `eiffel_native_http` and `eiffel_outbound_http`
both use `StatefulRouter<Counter>`.

### 7. Bridge specimen cleanup — landed in 059 Rock 7

Closed by Phase 059 Rock 7. `eiffel_axum_counter` and
`eiffel_ws_room` rewritten to the specimens-rule shape:
`src/lib.rs` with shared types/scripted client, top-level
`tokio_impl.rs` / `tina_impl.rs`, `main.rs` dispatcher,
`tests/smoke.rs`. The `src/comparison/` harness directories are gone.
Both still use the blessed `BridgeHost::new` / `register_bridge` /
`drain_and_shutdown` lifecycle.

Follow-up bridge polish rebased the HTTP-shaped bridge specimens onto
`tina_tower_bridge::TinaTowerService`, then added the specimen-facing
`TinaService<M, R>` alias and re-exported Tower's `Service` trait.
The rough spots left are smaller but real: Axum handlers still use the
`let mut svc = svc;` Tower idiom, setup is still
`register_bridge(...)` then `TinaTowerService::new(...)`, and
WebSocket handlers need service clones because `Service::call` takes
`&mut self`.

### 8. RPC service topology beyond single — deferred (runtime prerequisite)

Investigated and deferred in Phase 059 Rock 8. A real concurrent
`PooledService` requires an isolate to hold *multiple* pending
`IsolateCall` continuations simultaneously (one per in-flight
`ServiceCall` the pool is dispatching to a worker). Today the
runtime stores `MessageCallContext` as a single `Option<...>` per
isolate, so a pool frontend would serialize through one-at-a-time
and not actually pool concurrent work. The unblocking work is at
the runtime level, not the rpc level. The sealed-stub
`PooledService` / `ShardedService` types stay in `tina_rpc` to
document the planned shape.

**Build:**

- runtime support for N pending `IsolateCall` continuations per
  isolate, then real `PooledService`;
- `ShardedService` after sharded primitives (053);
- explicit mailbox/capacity semantics for each;
- docs that say how `Full`, `Closed`, `Timeout`, and partial failure behave.

The registry should keep mapping service name to one address. The address may
be a single service, pool frontend, or shard router.

### 9. Uniform overload reports for pressure runners — landed in 059 Rock 9

Closed by Phase 059 Rock 9. `tina_runtime` ships
`PressureReport { side, accepted, full, closed, timeouts, other,
rss_peak_kb, exit }` plus `format_pressure_line(...)` that produces
the canonical line:

```text
pressure side=<name> accepted=N full=N closed=N timeouts=N other=N [rss_peak_kb=N] exit=<status>
```

`eiffel_real_io_chat` opts in and prints one line per side.
`eiffel_cpu_run` captures target stdout, intercepts `pressure ...`
lines, and re-emits them tagged by run label; non-pressure lines
pass through unchanged.

Chapter 17 ("Pressure Report Convention") in the user guide is the
blessed shape and explains why this is a *convention* (line
contract) rather than a *framework* (program contract).

### 10. Reqwest-bridge flatten edge: useful but per-call-site

**Surfaced by:** `eiffel_webhook_publisher`.

The `tina-reqwest-bridge` ergonomics polish shipped
`flatten_outcome(outcome) -> Result<R, ReqwestCallError>` as an
opt-in flat-error helper. Building a specimen that uses all three
call shapes (`send_request`, raw `call(addr, ReqwestMsg::Send(...))`,
and `send_request` + `flatten_outcome` at the reply translator) made
it clear that flattening is *useful* — the consumer-side match drops
from five arms to three without losing the bridge-vs-worker layer
naming — but the call-site syntax for shape 3 is denser than for
shapes 1 and 2:

```rust
.reply(DriverMsg::PostedViaSendRequest)                // shape 1: bare ctor
.reply(DriverMsg::PostedViaRawCall)                    // shape 2: bare ctor
.reply(|outcome| DriverMsg::PostedFlattened(flatten_outcome(outcome))) // shape 3: closure
```

A first-time reader has to look at shape 3 twice. Mixing layered
and flat call sites in the same isolate without a comment explaining
why some are layered is confusing.

**Build:**

- Keep `flatten_outcome` opt-in. Do not default it.
- Document explicitly: "pick layered or flat per call-site cluster,
  not per-isolate-mixed-mode."
- Consider a derive-style helper that produces a continuation enum
  variant + a bare-function translator from one declaration, so
  shape-3 call sites read the same as shapes 1/2. Not urgent —
  punt until a non-pedagogical user actually mixes the two and
  flinches.

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
- old shared comparison harnesses: examples are specimens, tests are proof;
- `Arc<Outcome>` / `Arc<Mutex<Vec<_>>>` for an isolate's *final* app
  value: use `stop_with(value)` + `runtime.observe_result::<T>(addr)?`
  (Phase 059 Rock 1).

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
