# Tracing

Tina records the right facts. Every `RuntimeEvent` carries shard,
isolate, generation, call id, kind, and a typed reason
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

- `Full`, `Closed`, `Timeout`, and the typed `*Rejected` reasons
  stay distinct in `reason`.
- IDs (`event_id`, `cause_id`, `call_id`, `slot_id`, `isolate`,
  `generation`, `child_isolate`, `record_index`) are correlation
  fields. Unbounded cardinality. Do not turn them into metric labels.
- Live shard `Running`/`Stopped`/`Failed` is the `state` field on
  `live_shard` events. Not a generic `up=1`.

## Optional dependency

`tina-tracing` is its own crate. `tina`, `tina-runtime`, `tina-sim`,
`tina-mailbox-spsc`, and `tina-supervisor` do not depend on it.

```toml
[dependencies]
tina-tracing = { path = "..." }       # or version = "..."
tracing-subscriber = "0.3"
```

## Field set

| Field        | Source                                                         |
|--------------|----------------------------------------------------------------|
| `kind`       | `RuntimeEventKind` variant name                                |
| `event_id`   | `RuntimeEvent::id`                                             |
| `cause_id`   | `RuntimeEvent::cause` — bare number, or `-` when absent        |
| `shard`      | `RuntimeEvent::shard`                                          |
| `isolate`    | `RuntimeEvent::isolate`                                        |

Per-kind extras:

- `effect` — handler-finished / effect-observed.
- `target_shard`, `target_isolate`, `target_generation` — sends.
- `call_id`, `call_kind` — call dispatch / completed / failed /
  completion-rejected / reply-rejected.
- `slot_id` — deferred-reply events.
- `child_isolate`, `failed_child`, `failed_ordinal`, `child_ordinal`,
  `old_isolate`, `old_generation`, `new_isolate`, `new_generation` —
  spawn / supervisor / restart.
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

`SendRejected{Closed}` is `WARN`. Closed is lifecycle truth, not an
error; the operator decides what to alert on.

## Wiring it up

### Live (preferred)

Wire the observer at build time; events flow into the subscriber as
they happen. Sync, in-line, on the recording thread.

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

Explicit-step `Runtime` has a setter for tests and tools:

```rust
runtime.set_trace_observer(Some(Arc::new(TracingObserver::new())));
```

Hot-path rules:

- one sync callback per event;
- runs on the recording thread;
- a panicking observer kills that thread — the runtime does not
  catch it;
- per-shard order preserved; cross-shard order is whatever the
  threads produce.

`TraceRetention::Off` + observer = stream-only mode: in-memory trace
stays empty, every event flows through the hook.

### End-of-run dump

For tests, tools, one-shot scripts that don't want a subscriber up
front.

```rust
use tina_tracing::{emit_events, emit_trace_snapshot};

// Single-shard / explicit-step Runtime returns &[RuntimeEvent]:
emit_events(runtime.trace().iter());

// ThreadedRuntime / LocalSystem return a TraceSnapshot. Use the
// snapshot helper so partial results emit one warn-level
// `kind="trace_snapshot_partial" missing_shards=[…]` first, instead
// of being silently dropped.
emit_trace_snapshot(&snapshot);
```

Per-snapshot live topology dump:

```rust
use tina_tracing::emit_snapshot;

// e.g. on a host-side timer or after shutdown
let topology = runtime.topology();
emit_snapshot(&topology);
```

## Bridges

`tina-rpc-tokio`, `tina-tokio-bridge`, `tina-tower-bridge`,
`tina-reqwest-bridge`, and `tina-sqlite-bridge` each ship an
optional `tracing` Cargo feature with the same shape:

```toml
[dependencies]
tina-sqlite-bridge = { version = "...", features = ["tracing"] }
```

What emits today:

- `tina-rpc-tokio`: bridge spans on `tina_rpc.bridge.call` with
  `service`, `method`, `correlator`, `result_kind`.
- `tina-sqlite-bridge`: per-call events on `tina_sqlite.bridge.call`
  with `kind`, `request_kind`, `reason`, `outcome`, `rows_changed`,
  `row_count`, `elapsed_ms`; plus one `kind="close"` lifecycle event
  on `tina_sqlite.bridge`. `reason` reuses the runtime vocabulary
  (`Full`, `Closed`, `Timeout`) where the concept matches and adds
  bridge-specific names (`Busy`, `Constraint`, `Io`, `Sqlite`,
  `ResponseTooLarge`, `InvalidRequest`, `Internal`).
- `tina-tokio-bridge`, `tina-tower-bridge`, `tina-reqwest-bridge`:
  feature scaffolded, no spans yet.

A follow-up pass extends emission to the remaining three bridges
and reconciles any drift across the five. Targets stay
`tina_<bridge>.…`; runtime events stay on `tina_runtime::trace`.

Correlate runtime `call_id` ↔ bridge correlator by hand for now.
Bridge fields stay bridge-shaped, not runtime-shaped.

## Not in scope

- Installing a global subscriber. Only
  `install_global_default_subscriber` (behind the `subscriber`
  feature) does, and the verb is in the name.
- OpenTelemetry / Prometheus mappers. Metrics policy is a separate
  phase. Two crates is coincidence; three repeated shapes is evidence.
- Span timing for runtime calls. Tina's call lifecycle is already a
  causal chain in the trace.
- New event kinds. The adapter is a strict reader.

For shorter logs filter on `target = "tina_runtime::trace"` or
`target = "tina_runtime::live"`. For richer logs, layer your own
`tracing` spans around your application code — runtime events sit
underneath.
