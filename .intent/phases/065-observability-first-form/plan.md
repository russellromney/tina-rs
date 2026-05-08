# 065 Observability First Form

## Status

- Done: `tina-tracing` crate (events + live snapshot + trace-snapshot
  partial marker + stable-name re-exports + `TracingObserver`),
  doc page, runnable example. `cause_id` renders as bare number /
  `-`. `TraceObserver` trait on `tina-runtime` with sync inline hook,
  builder-time wiring on `ThreadedRuntime` / `ThreadedMultiShardRuntime`
  / `LocalSystem*Builder`, sim parity, and a noop-observer trace
  byte-equality test on both runtime and sim.
- Deferred: OpenTelemetry, Prometheus, exporter policy, metric policy,
  cross-version on-the-wire trace formats, span hierarchy beyond
  shard+isolate scoping. **Bridge tracing alignment** is scaffolded
  (every bridge — `tina-tokio-bridge`, `tina-tower-bridge`,
  `tina-reqwest-bridge`, `tina-rpc-tokio`, `tina-sqlite-bridge` —
  now ships an optional `tracing` feature with the same shape) but
  no new spans/events are emitted yet; the next pass picks the shared
  field vocabulary.

## Goal

Make Tina runtime truth flow into normal Rust observability tools without
flattening it.

Tina already records the right facts: `RuntimeEvent` carries shard, isolate,
generation, call id, kind, and the typed reasons (`Full`, `Closed`,
`Timeout`, `CallerClosed`, `ResourceClosed`, `DeferredReplyRejectedReason`,
`SupervisionRejectedReason`, `CallCompletionRejectedReason`,
`CallReplyRejectedReason`, `RestartSkippedReason`). `LiveTopologyReport`
carries per-shard `LiveShardState` and per-queue accepted/full/closed
counts. The bridges (`tina-reqwest-bridge`, `tina-tower-bridge`,
`tina-rpc-tokio`) carry their own outcome enums and counter snapshots.

The cost today is that this truth lives behind typed Rust APIs only.
A service operator running Tina under `tracing_subscriber` sees nothing
unless they walk the trace themselves.

065 builds the smallest adapter that turns `RuntimeEvent` slices and live
report snapshots into `tracing::Event`s and `tracing::Span`s with structured
fields. Tina-side enums become exact field values, not flattened
"request_failed" strings.

## Rule

```text
ergonomics may surface truth.
ergonomics may not flatten truth.
```

The adapter is allowed to:

- emit one tracing event per runtime event;
- pick `Level::TRACE` / `DEBUG` / `INFO` / `WARN` / `ERROR` per event kind;
- attach correlation fields (`event_id`, `cause_id`, `shard`, `isolate`,
  `generation`, `call_id`, `slot_id`, `child_isolate`, `target_*`);
- attach the typed reason as a stable string field (`"Full"`, `"Closed"`,
  `"CallerClosed"`, `"ReplyPathFull"`, …) — not collapsed.

The adapter must not:

- merge `Full` and `Closed` into `error`;
- merge caller `Timeout` and explicit cancel;
- attach IDs as metric labels;
- silently ship a global subscriber;
- pull `tracing` into `tina-runtime` as a required dep.

> Trace-level truth is structured. Metrics policy is a separate phase.
> Tina ids are correlation fields. They are not metric labels.

## Non-Goals

- No exporter policy. The user installs whatever `tracing_subscriber` they
  want.
- No OpenTelemetry, no Prometheus mapper. Those wait on a later phase that
  defines a metrics vocabulary; the rule is "two crates is coincidence,
  three repeated shapes is evidence."
- No async/Tokio-specific span timing. Tina's call lifecycle is already in
  `RuntimeEvent`; spans here are cheap correlation scopes, not timed work.
- No global subscriber side effects unless the function name says
  `install_global_*` and the caller asks for it.
- No new event kinds. The adapter is a strict reader.
- No `tina-runtime` API change. `RuntimeEvent` and the live reports are
  already the source of truth.
- No new dependency on `tracing` from `tina`, `tina-runtime`, `tina-sim`,
  `tina-mailbox-spsc`, or `tina-supervisor`.

## Design Decision: New Crate

A new crate `tina-tracing` is the right home.

Reasoning:

- "Do not make tracing required for tina-runtime" is a hard rule. Even with
  a Cargo feature, putting the conversion inside `tina-runtime` mixes core
  semantics with optional ecosystem glue.
- Dependencies flow concrete → abstract. `tina-tracing` depends on
  `tina-runtime` types (`RuntimeEvent`, kinds, `LiveTopologyReport`); no
  reverse dep.
- The bridge crates (e.g. `tina-rpc-tokio`) already use `tracing` for
  bridge-internal call spans. That is a separate concern; `tina-tracing` is
  specifically for converting Tina runtime trace events and live snapshots
  into structured `tracing` records. The bridges keep their own optional
  `tracing` feature for now; a follow-up rock may consolidate field names.
- A new crate makes the dep direction obvious in the workspace: anything
  that does not need tracing simply does not depend on `tina-tracing`.

A `tina-runtime` Cargo feature was considered and rejected because it would
add a hidden compile-time switch on the most central crate; users wiring
`tracing` would still need to know which feature flag turns it on, and we
would still need a dedicated module. Given the new crate is small and
self-contained, the crate boundary is the cheaper place to draw the line.

## First-Form Field Set

Every emitted event carries this base set:

- `event_id: u64` — `RuntimeEvent::id`.
- `cause_id: u64` — present when `RuntimeEvent::cause` is `Some`.
- `shard: u32` — `RuntimeEvent::shard`.
- `isolate: u64` — `RuntimeEvent::isolate`.
- `kind: &'static str` — stable kind name (`"send_rejected"`, etc.).

Per-kind extras (only when the kind carries them):

- `target_shard`, `target_isolate`, `target_generation` — for
  send-dispatch / send-accepted / send-rejected.
- `call_id`, `call_kind` — for call dispatch / completed / failed /
  completion-rejected / reply-rejected, and for deferred-reply events.
- `slot_id` — for deferred-reply events.
- `child_isolate`, `failed_child`, `failed_ordinal`, `child_ordinal`,
  `old_isolate`, `old_generation`, `new_isolate`, `new_generation` — for
  spawn / supervisor / restart events.
- `effect: &'static str` — for handler-finished / effect-observed.
- `reason: &'static str` — typed reason name when present
  (`"Full"`, `"Closed"`, `"CallerClosed"`, `"ReplyPathFull"`,
  `"RequesterShardClosed"`, `"TypeMismatch"`, `"NoPendingCall"`,
  `"MailboxFull"`, `"RequesterClosed"`, `"ResourceClosed"`,
  `"NotRestartable"`, `"BudgetExceeded"`, `"SupervisorStopped"`,
  reqwest/rpc/runtime call-error names).
- `attempted_restart`, `max_restarts` — for `BudgetExceeded` only.
- `record_index` — for journal events.
- `error: &'static str` — for runtime call errors (`CallError` variant
  name).

`event_id`, `cause_id`, `call_id`, `slot_id`, `isolate`, `generation`,
`*_isolate`, `*_generation`, `child_*`, `failed_ordinal`, `child_ordinal`,
`record_index`, `attempted_restart`, `max_restarts` are *correlation*
fields. They must not be used as metric labels (cardinality is unbounded).
This is documented on the crate root and on the doc page.

## Level Mapping (first form)

- `TRACE`: `MailboxAccepted`, `HandlerStarted`, `HandlerFinished`,
  `EffectObserved`, `SendDispatchAttempted`, `SendAccepted`,
  `CallDispatchAttempted`, `CallCompleted`, `Spawned`,
  `RestartChildAttempted`, `RestartChildCompleted`,
  `DeferredReplyCaptured`, `DeferredReplySent`, `DeferredReplyDropped`,
  `JournalAppended`, `SnapshotCommitted`, `RecoveryStarted`,
  `RecoveryFinished`, `MessageAbandoned`.
- `DEBUG`: `IsolateStopped`, `RestartChildSkipped`,
  `SupervisorRestartTriggered`.
- `WARN`: `SendRejected`, `CallCompletionRejected`,
  `CallReplyRejected`, `DeferredReplyRejected`,
  `SupervisorRestartRejected`, `JournalAppendFailed`,
  `SnapshotCommitFailed`.
- `ERROR`: `HandlerPanicked`, `RecoveryFailed`, `CallFailed`.

`SendRejected{Closed}` is `WARN`, not `ERROR`. Closed is good lifecycle
truth, not a crash; the operator decides what to alert on.

## Rocks

### Rock 1: New Crate Skeleton

Add `tina-tracing` to the workspace.

`Cargo.toml`:

- `[dependencies] tina = path`, `tina-runtime = path`,
  `tracing = { version = "0.1", default-features = false, features = ["std"] }`.
- `[dev-dependencies] tracing-subscriber`, `tracing-test`.
- No `optional` on `tracing`; this crate's whole job is tracing.
- No `tokio`, `serde`, or async deps.

`src/lib.rs` skeleton:

- crate-level doc page describing the rule, the field set, and the
  "not metrics labels" warning;
- `#![forbid(unsafe_code)]`;
- `#![deny(missing_docs)]` (consistent with the rest of the workspace);
- two public modules: `events` (per-`RuntimeEvent` emission) and
  `live` (per-`LiveTopologyReport` snapshot emission).

Proof:

- `cargo build -p tina-tracing` succeeds.
- crate has no transitive dep on Tokio.

### Rock 2: Lifecycle Event Mapping

Map the lifecycle and dispatch slice of `RuntimeEventKind`:

- `MailboxAccepted`, `HandlerStarted`, `HandlerPanicked`, `HandlerFinished`,
  `EffectObserved`, `IsolateStopped`, `MessageAbandoned`, `Spawned`,
  `SupervisorRestartTriggered`, `SupervisorRestartRejected`,
  `RestartChildAttempted`, `RestartChildSkipped`, `RestartChildCompleted`.

Public API:

```rust
pub fn emit_event(event: &RuntimeEvent);
pub fn emit_events<'a, I: IntoIterator<Item = &'a RuntimeEvent>>(events: I);
```

`emit_event` runs at the level chosen by the kind. No span entry/exit.
Field names are stable strings; reason names match the Rust variant name
verbatim.

Rules:

- `kind` field is the stable name (e.g. `"handler_finished"`,
  `"supervisor_restart_rejected"`).
- `effect` is one of the stable `EffectKind` names
  (`"reply"`, `"send"`, `"call"`, `"batch"`, `"stop"`, `"stop_with"`,
  `"reply_to"`, `"spawn"`, `"restart_children"`, `"noop"`).
- `RestartPolicy` becomes `policy = "OneForOne"` etc.
- The `cause_id` field is omitted when `RuntimeEvent::cause` is `None`;
  it is never zero-as-absent.

Proof:

- unit test feeds a synthetic 5-event handler lifecycle and asserts every
  expected field via `tracing-test`'s captured-event helper;
- per-kind mapping table test verifies level + kind name for every
  handler/supervisor/restart variant.

### Rock 3: Pressure Event Mapping

Map the pressure / rejection slice:

- `SendRejected`, `CallCompletionRejected`, `CallReplyRejected`,
  `DeferredReplyRejected`, `DeferredReplyDropped`, `DeferredReplyCaptured`,
  `DeferredReplySent`.

Reason field rules:

- `SendRejectedReason::Full` → `reason = "Full"`.
- `SendRejectedReason::Closed` → `reason = "Closed"`.
- `CallCompletionRejectedReason::MailboxFull` → `reason = "MailboxFull"`,
  `RequesterClosed` → `"RequesterClosed"`, `ResourceClosed` →
  `"ResourceClosed"`.
- `CallReplyRejectedReason::NoPendingCall` → `reason = "NoPendingCall"`,
  `ReplyPathFull` → `"ReplyPathFull"`,
  `RequesterShardClosed` → `"RequesterShardClosed"`.
- `DeferredReplyRejectedReason::CallerClosed` → `"CallerClosed"`,
  `ReplyPathFull` → `"ReplyPathFull"`,
  `RequesterShardClosed` → `"RequesterShardClosed"`,
  `TypeMismatch` → `"TypeMismatch"`.

Proof:

- one synthetic pressure trace covers each rejection reason; the captured
  events assert kind + level + reason field values exactly;
- order-preserving: `emit_events` enters fields in iteration order.

### Rock 4: Call & Resource Correlation

Map call and resource correlation fields:

- `CallDispatchAttempted`, `CallCompleted`, `CallFailed`,
  `CallCompletionRejected`, `CallReplyRejected`, journal/snapshot/recovery
  events.

Field rules:

- `call_kind` is the lower-snake_case form of `CallKind`
  (`"tcp_bind"`, `"tcp_accept"`, `"tcp_read"`, `"udp_send_to"`,
  `"tls_handshake"`, …) so log filters can stay short;
- `error = "<CallError variant name>"` for `CallFailed`;
- `record_index` for journal events;
- `slot_id` for deferred slot events.

Proof:

- per-`CallKind` mapping test covers every variant currently exposed by
  `tina-runtime::CallKind`. The test fails if a new variant is added to
  the runtime without being mapped here. (Compiler-driven via exhaustive
  match.)

### Rock 5: Live Topology Snapshot

Add `live::emit_snapshot(report: &LiveTopologyReport, span_name: Option<&str>)`.

Behavior:

- emits one event per shard at level `INFO` if `state == Running`,
  `WARN` if `Stopped`, `ERROR` if `Failed`. Fields:
  `shard`, `state`, `worker_name`, `worker_thread_id`, `configured_core`,
  `observed_core`, `affinity_status`, `ingress_capacity`,
  `ingress_accepted`, `ingress_rejected_full`, `ingress_rejected_closed`,
  `storage_lane_capacity`, `dns_lane_capacity`, `tls_lane_capacity`,
  `process_lane_capacity`, `signal_lane_capacity`, `trace_retention`,
  `trace_dropped`, `owned_resource_count`,
  `worker_held_resource_count`, `pending_driver_call_count`.
- emits one event per remote queue at `DEBUG` (or `WARN` if any
  `rejected_full` is non-zero). Fields: `source`, `target`, `capacity`,
  `accepted`, `rejected_full`, `rejected_closed`.
- `Optional<usize>` becomes a missing field, never `0`.

This is a *snapshot* helper. The caller decides when to call it (on
shutdown report, on a periodic timer in the host, etc.). The crate does
not start its own thread or timer.

Proof:

- a synthetic single-shard `LiveTopologyReport` produces one shard event
  with the expected `state` and ingress fields;
- a synthetic multi-shard report with one `Failed` shard surfaces at
  `ERROR` for that shard only.

### Rock 6: Optional Convenience: install_global_default

```rust
pub fn install_global_default_subscriber() -> Result<(), tracing::dispatcher::SetGlobalDefaultError>
```

Rules:

- name says `install_global_default_subscriber`. The verb is in the name;
  no implicit install anywhere else.
- uses `tracing_subscriber::FmtSubscriber` behind a feature flag
  `subscriber` to avoid pulling `tracing-subscriber` into every consumer.
- documented as "examples and quick demos only; production services should
  install their own subscriber."

Proof:

- doctest behind `--features subscriber` shows the install pattern.
- without the feature, the function does not exist.

### Rock 7: Docs And Example

Add one doc page `docs/tina-user-guide/19-tracing.md`:

- crate is optional;
- field set;
- ID fields are correlation only — do **not** flatten into metric labels;
- `Full` / `Closed` / `Timeout` / typed reason names stay distinct;
- snippet that wires `tracing_subscriber::fmt` and walks an end-of-run
  trace via `emit_events`;
- snippet that calls `live::emit_snapshot` from a host-side polling loop.

Update `docs/tina-user-guide/README.md` to list the new entry.

Update one Eiffel example to print structured tracing output. Pick
`eiffel_outbound_http` (already exercises bridge + late reply) or
`eiffel_supervised_worker` (lifecycle + restart). The example wires
`tracing_subscriber::fmt` and calls `tina_tracing::events::emit_events`
on the captured trace before the runtime drops; the example output
gains structured `kind`, `reason`, `call_id`, `shard`, `isolate` lines.

Do **not** make the example require the bridge's existing
`tracing` feature; this is the runtime-trace adapter, not the bridge's
internal spans.

Proof:

- doc compiles (markdown only, but referenced symbols exist);
- example runs with `cargo run --example ...` and prints at least one
  `kind=send_rejected reason=Full` or equivalent line under load;
- README link list includes the new doc page.

## Order

1. Rock 1: crate skeleton, workspace member, no logic.
2. Rock 2: lifecycle event mapping + per-kind mapping test.
3. Rock 3: pressure event mapping + per-reason mapping test.
4. Rock 4: call / resource correlation + exhaustive `CallKind` match.
5. Rock 5: live snapshot emitter.
6. Rock 7: doc page + example. (Rock 6 is optional and may slip.)
7. Rock 6: `install_global_default_subscriber` behind `subscriber`
   feature, only if it makes the example shorter.

## Done Means

- `tina-tracing` crate exists in the workspace with `cargo build -p
  tina-tracing` and `cargo test -p tina-tracing` green.
- Every `RuntimeEventKind` variant is mapped; the mapping is exhaustive
  via `match` so new variants force a compile error here.
- Every typed reason (`Full`, `Closed`, `Timeout`, `CallerClosed`,
  `ResourceClosed`, `ReplyPathFull`, `RequesterShardClosed`,
  `RequesterClosed`, `MailboxFull`, `NoPendingCall`, `TypeMismatch`,
  `BudgetExceeded`, `SupervisorStopped`, `NotRestartable`) appears as a
  distinct stable string in the structured output; tests pin this.
- `LiveTopologyReport` snapshot emitter reports per-shard state and
  per-queue accepted/full/closed without flattening.
- `tracing` is **not** a required dep of `tina`, `tina-runtime`, `tina-sim`,
  `tina-mailbox-spsc`, or `tina-supervisor`. `cargo tree` confirms.
- A doc page lives at `docs/tina-user-guide/19-tracing.md` and is linked
  from the user-guide README.
- One Eiffel example prints structured tracing output for a real run.
- No new event kinds added to the runtime. No global subscriber installed
  unless the caller calls `install_global_default_subscriber`.

## Bridge Tracing Scaffolding (Follow-Up Note)

`tina-rpc-tokio` already shipped a `tracing` feature with its own
field names (`service`, `method`, `correlator`, `result_kind`).
`tina-tokio-bridge`, `tina-tower-bridge`, and `tina-reqwest-bridge`
now ship the same optional `tracing = ["dep:tracing"]` shape with no
spans/events emitted yet.

That gives us four crates with the same surface, ready for a single
alignment pass instead of four uncoordinated dialects.

The next pass picks the shared vocabulary. Likely shape:

- one span per bridge admission attempt (`tina_<bridge>.admit`).
- one span per bridge call lifetime (`tina_<bridge>.call`) when the
  bridge owns a call/correlator concept.
- shared field names where the concept is the same:
  - `reason = "Full" | "Closed" | "Timeout" | "ResourceClosed" | …`
    matches the runtime's typed-reason vocabulary;
  - `bridge` carries the concrete bridge name as a stable string;
  - `correlator` (if any) stays a correlation field — never a metric
    label.
- bridge-specific fields stay bridge-specific (`service`/`method` for
  RPC; `method`/`url_host` for reqwest; `tower_layer` for tower) and
  do not pollute runtime events.

Rules carried forward:

- bridges still do not pull `tracing` in unless their caller turns
  the feature on;
- typed `BridgeError`/`ReqwestError`/`tower::Status`-shaped values
  must continue to surface, not be flattened to a generic `error`;
- `tina-tracing` stays the runtime-trace adapter; bridge tracing
  ships from each bridge's own crate to keep dep direction clean.

Proof for the alignment pass (separate phase):

- one runnable example shows runtime + bridge spans interleaved in a
  single subscriber stream, sharing the `reason` vocabulary;
- `cargo tree` shows no bridge depends on another bridge's tracing
  feature.
