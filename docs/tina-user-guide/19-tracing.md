# Tracing

Tina records the right facts. Every `RuntimeEvent` carries shard,
isolate, generation, call id, kind, and a *typed* reason
(`Full`, `Closed`, `CallerClosed`, `ReplyPathFull`,
`RequesterShardClosed`, `MailboxFull`, `RequesterClosed`,
`NoPendingCall`, `TypeMismatch`, `BudgetExceeded`,
`SupervisorStopped`, `NotRestartable`, …).

`tina-tracing` is the boring shim that turns those facts into
`tracing::Event`s with structured fields. It does not flatten.

## Rule

```text
ergonomics may surface truth.
ergonomics may not flatten truth.
```

- `Full`, `Closed`, `Timeout`, and the typed `*Rejected` reasons stay
  distinct in the `reason` field.
- IDs (`event_id`, `cause_id`, `call_id`, `slot_id`, `isolate`,
  `generation`, `child_isolate`, `record_index`) are *correlation*
  fields. They are unbounded cardinality. **Do not** turn them into
  metric labels in your subscriber.
- Live shard `Running`/`Stopped`/`Failed` is the `state` field on
  `live_shard` events. It is not a generic `up=1`.

## Optional dependency

`tina-tracing` is a separate crate. `tina`, `tina-runtime`, `tina-sim`,
`tina-mailbox-spsc`, and `tina-supervisor` do **not** depend on it. A
service operator who wants tracing adds:

```toml
[dependencies]
tina-tracing = { path = "..." }       # or version = "..."
tracing-subscriber = "0.3"
```

## Field set (first form)

Every event from `tina-tracing` carries:

| Field        | Source                                                         |
|--------------|----------------------------------------------------------------|
| `kind`       | `RuntimeEventKind` variant name                                |
| `event_id`   | `RuntimeEvent::id`                                             |
| `cause_id`   | `RuntimeEvent::cause` — bare number when present, `-` when not |
| `shard`      | `RuntimeEvent::shard`                                          |
| `isolate`    | `RuntimeEvent::isolate`                                        |

`cause_id` never uses `0` as a sentinel for "no cause". Root events
render as `cause_id=-`.

Per-kind extras:

- `effect` — handler-finished / effect-observed.
- `target_shard`, `target_isolate`, `target_generation` —
  send-dispatch / send-accepted / send-rejected.
- `call_id`, `call_kind` — call dispatch / completed / failed /
  completion-rejected / reply-rejected.
- `slot_id` — deferred-reply events.
- `child_isolate`, `failed_child`, `failed_ordinal`, `child_ordinal`,
  `old_isolate`, `old_generation`, `new_isolate`, `new_generation` —
  spawn / supervisor / restart events.
- `reason` — typed reason name when present.
- `attempted_restart`, `max_restarts` — `BudgetExceeded` only.
- `record_index` — journal events.
- `error` — `CallError` variant name.

## Levels

| Level | Kinds                                                   |
|-------|---------------------------------------------------------|
| TRACE | mailbox accept, handler start/finish, effect observed, send dispatch/accept, call dispatch/completed, deferred capture/send/drop, journal appended, snapshot committed, recovery start/finish, restart child attempted/completed |
| DEBUG | isolate stopped, restart child skipped, supervisor restart triggered |
| WARN  | send rejected, call completion rejected, call reply rejected, deferred reply rejected, supervisor restart rejected, journal append failed, snapshot commit failed |
| ERROR | handler panicked, recovery failed, call failed |

`SendRejected{Closed}` is `WARN`, not `ERROR`. Closed is benign
lifecycle truth; the operator decides what to alert on.

## Wiring it up

### Live (preferred): wire the observer at build time

Every recorded event flows into your fmt subscriber as it happens.
No end-of-run dump needed. The hook is sync, in-line, on the
recording thread.

```rust
use std::sync::Arc;
use tina_runtime::{ThreadedRuntime, ThreadedRuntimeConfig};
use tina_tracing::TracingObserver;
use tracing_subscriber::FmtSubscriber;

let subscriber = FmtSubscriber::builder()
    .with_max_level(tracing::Level::DEBUG)
    .finish();
tracing::subscriber::set_global_default(subscriber).unwrap();

let runtime = ThreadedRuntime::with_config_and_trace_observer(
    shard,
    factory,
    ThreadedRuntimeConfig::default(),
    Arc::new(TracingObserver::new()),
);
```

`LocalSystem` builders take the same observer:

```rust
let app = LocalSystem::single_shard(shard, factory)
    .trace_observer(Arc::new(TracingObserver::new()))
    .build();
```

The explicit-step `Runtime` exposes a setter for tests and tools:

```rust
runtime.set_trace_observer(Some(Arc::new(TracingObserver::new())));
```

Hot-path rules:

- one synchronous callback per event;
- runs on the recording thread (shard worker for live, stepper for
  explicit-step / sim);
- a panicking observer kills that thread — the runtime does not
  catch it;
- per-shard order is preserved; cross-shard order across the live
  multi-shard runtime is whatever the threads produce.

Setting `TraceRetention::Off` together with an observer gives a
**stream-only** mode: the in-memory trace stays empty and every
event flows straight through the hook.

### End-of-run trace dump

Useful for tests, tools, and one-shot scripts that don't want to
wire a subscriber up front.

```rust
use tina_tracing::{emit_events, emit_trace_snapshot};

// Single-shard / explicit-step Runtime returns &[RuntimeEvent]:
emit_events(runtime.trace().iter());

// ThreadedRuntime / LocalSystem return a TraceSnapshot. Use the
// snapshot helper so a partial result becomes one warn-level
// `kind="trace_snapshot_partial" missing_shards=[…]` event before
// the rest, instead of being silently dropped.
emit_trace_snapshot(&snapshot);
```

Per-snapshot live topology dump:

```rust
use tina_tracing::live::emit_snapshot;

// e.g. on a host-side timer or after shutdown
let report = runtime.topology_report().expect("topology report");
emit_snapshot(&report);
```

## Bridges

`tina-rpc-tokio`, `tina-tokio-bridge`, `tina-tower-bridge`, and
`tina-reqwest-bridge` each expose an optional `tracing` Cargo
feature with the same shape:

```toml
[dependencies]
tina-rpc-tokio = { version = "...", features = ["tracing"] }
```

`tina-rpc-tokio` already emits bridge spans
(`tina_rpc.bridge.call` with `service`, `method`, `correlator`,
`result_kind`); the other three currently scaffold the feature
flag without emitting yet. A follow-up pass aligns the field
vocabulary across all four so a single subscriber sees consistent
records — runtime trace events under
`target = "tina_runtime::trace"` and bridge spans under
`target = "tina_<bridge>.…"` — sharing the same `reason` strings
where the concept matches.

Until that lands: filter on the targets you have, correlate
runtime `call_id` ↔ bridge correlator by hand, and treat bridge
fields as bridge-shaped, not runtime-shaped.

## What this crate does *not* do

- Install a global subscriber. The only function that does is
  `install_global_default_subscriber` (behind the `subscriber`
  feature), and the verb is in the name.
- Map Tina events to OpenTelemetry / Prometheus. Metrics policy is a
  separate phase. The rule: two crates is coincidence, three repeated
  shapes is evidence.
- Span timing for runtime calls. Tina's call lifecycle is already a
  causal chain in the trace; the adapter does not invent extra spans.
- Add new event kinds to the runtime. The adapter is a strict reader.

If you want shorter logs, filter on `target = "tina_runtime::trace"`
or `target = "tina_runtime::live"`. If you want richer logs, layer
your own `tracing` spans around your application code; the runtime
events will sit cleanly underneath.
