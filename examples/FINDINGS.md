# Specimen Findings — Current Product Work

This file is the current action list. Examples are specimens: they
show how Tokio and Tina code feel for the same kind of job. When the same
Tina pain appears across specimens, it becomes runtime/API work here.

The active list below is what Tina still needs. Earlier rounds that
have closed are summarized further down so external references stay
valid; the long-form history lives in
[`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md).

## Active

Finding numbers are stable across phases — when a finding closes it
moves to the [Closed](#closed) section below with the same number.

### 2. ScatterCoord setup is heavy for the happy path

**Surfaced by:** `specimen_sharded_fanout_read`.

A bounded scatter/gather over three shards needs:

- coord isolate registration with `ScatterCoordMsg::{Bind, Start, Reply}`;
- a `ReplyAdapter<ShardReply, ScatterCoordMsg, S>` registration and
  `From<ShardReply> for ScatterCoordMsg` impl;
- a `Bind { bridge }` send before the `Start`;
- caller-owned `pending_targets` / `outcomes` bookkeeping until every
  target is in.

That is the right *shape* for the rich pressure form (per-target timer,
aggregate timer, partial outcomes), but the ceremony is the same for the
"three shards, all reply, sum the results" case. The per-call-site setup is
roughly the size of the actual scatter/gather logic.

**Build:** a small `scatter_gather!` builder or a
`ScatterCoord::register(table, config, on_complete)` helper that wires the
adapter, the bind/start handshake, and the `pending_targets` /
`outcomes` accumulator at the same shard the coord lives on. Must keep the
typed partial-outcome surface — convenience may not collapse `Full` /
`Closed` / `Timeout` into one bucket.

### 3. Self-address at registration time

**Surfaced by:** `specimen_sharded_fanout_read`,
`specimen_dynamic_worker_pool`.

The `ReplyAdapter` pattern needs the coord's own address to wire the
adapter, and the coord needs the adapter's address before it can fan out.
Today the answer is a `Bind { bridge }` (or `Begin { self_addr }`) message
before `Start`. That works but adds a variant whose only job is to land
"you, isolate, look here for your replies" into the isolate's state.

**Build:** a way for an isolate to learn its own typed address at register
time — for example, a constructor closure parameter `|self_addr| {
ScatterCoord { ..., self_addr } }`. Avoids the bind-before-start handshake
and removes the `Option<Address<...>>` field that's only `None` for one
turn.

Self-address half shipped on the single-shard runtimes:
`Runtime::register_with_capacity_using(cap, |self_addr| ...)` and
the threaded mirror. `specimen_dynamic_worker_pool` migrated to it;
the chicken-and-egg `Begin { self_addr }` variant is gone.
Multi-shard parity (`MultiShardRuntime` /
`ThreadedMultiShardRuntime` / simulator) is deferred until a
multi-shard example needs it.

Still open: the cross-isolate handshake half — `Bind { bridge }` in
`specimen_sharded_fanout_read` is *not* about self-address, it's about
two isolates needing each other's addresses at registration. That
needs a paired-registration primitive or a different shape.

### 7. Reqwest-bridge flatten edge: useful but per-call-site

**Surfaced by:** `specimen_webhook_publisher`.

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

### 8. External cancellation API — first form shipped

**Surfaced by:** `specimen_cancellation_chain`.

**Resolved (Tina cancellation phase):** Tina now ships
`call_with_handle(addr, msg, t).reply(...)` returning a caller-owned
`CallHandle`, plus `cancel_call(handle).reply(...)` that closes one
pending isolate call's wait. The handle is move-only and not `Clone`.
Cancellation is visible truth: `CancelOutcome` (`Cancelled` /
`AlreadyCompleted` / `AlreadyCancelled` / `NotDispatched` /
`WrongRequester` / `CrossShardUnsupported`) is `#[must_use]`, and
late callee replies surface as `CallReplyRejected { CallerCancelled }`
or `DeferredReplyRejected { CallerCancelled }` events.

**Still open:** runtime-level `runtime.cancel_isolate(addr)` (third
form — closes every call an isolate owns) is a small wrapper around
`cancel_call`; deferred until the bounded `PendingCallSet` lands in
phase 067.

### 9. Drain helper for `PendingReplies` at service stop

**Surfaced by:** `specimen_graceful_pool_shutdown`,
`specimen_graceful_drain_server`.

`PendingReplies::drain()` returns `Vec<(K, DeferredReply<R>)>`,
which the user has to map into `Effect::Batch(reply_to(slot,
value))` calls plus a final `stop()`. The service-stop pattern
is identical at every call site:

```rust
let mut effects: Vec<_> =
    self.pending.drain().into_iter().map(|(_, slot)| reply_to(slot, R::Closed)).collect();
effects.push(stop());
Effect::Batch(effects)
```

The same area also wants a *deadline* — a drain that says "finish
in-flight work, but give up after T". Today that's a hand-rolled
`DrainDeadlineFired` continuation message scheduled via `sleep`
plus a check in the isolate's "is it done" predicate that returns
true on deadline-fired even when `pending > 0`. The
`tina-tokio-bridge::BridgeShutdownReport::drained_within_timeout`
flag is the bridge-side version of the same idea.

**Build:**

- ~~`pending.drain_into_effect(R::Closed) -> Effect<I>` (or
  similarly named) that returns the matching `Effect::Batch` in
  one call, with the trailing `stop()` opt-in via a sibling
  `drain_into_stop_effect(R::Closed)`.~~ Shipped:
  `PendingReplies::drain_replies` / `drain_replies_with` /
  `drain_replies_into_effect` / `drain_replies_into_stop` /
  `drain_replies_with_into_effect` /
  `drain_replies_with_into_stop`, all typed so a
  `PendingReplies<K, R>` only produces `Effect<I>` when
  `I::Reply = R`. `specimen_graceful_pool_shutdown` uses
  `pending.drain_replies_into_stop::<Self>(R::Closed)`. The
  deadline half of this finding (DrainGate) folds into
  finding 15 (Deadline as first-class context).
- An isolate-state `DrainGate` helper that holds the deadline +
  the pending-count predicate, with an `is_done` /
  `drained_within_timeout` accessor that the handler reuses.

### 11. Multi-stage pipeline ergonomics

**Surfaced by:** `specimen_two_stage_pipeline`.

A 3-stage pipeline reads as 4 enum variants in `PipelineMsg`
(Submit + Parsed + Validated + Executed), each with its own match
arm. The Tokio side reads as `parse(i).await?; validate(p).await?;
execute(v).await?` — three lines. The Tina version is correct and
trace-visible at every stage, but the variant count grows
linearly with stage count.

**Decision:** do not build a pipeline helper yet. The long form is
not merely noise: it names each suspension point and each
per-stage `Full` / `Closed` / `Timeout` edge. A helper that makes
Tina look like fake `async` would be worse for humans and LLMs.

**Revisit only if:** a non-pedagogical pipeline repeats enough
boilerplate that a helper can delete plumbing while keeping every
stage, timeout, and partial-progress fact visible. The raw
match-state-machine form remains semantic truth.

### 12. Rust footgun replication: shared receiver in worker pool

**Surfaced by:** `specimen_graceful_pool_shutdown` (Tokio side).

Not a Tina finding per se — but worth recording as the *kind of
footgun* Tina structurally avoids. The Tokio shutdown path needs
both `JoinSet::abort_all` AND `drop(rx_arc)`. Forgetting the
second leaves buffered jobs (and their reply oneshots) alive,
blocking queued callers forever. The test passes under low burst
because all jobs were in flight.

Tina's `pending.drain()` + `Effect::Batch(reply_to)` makes this
class of bug structurally impossible: every captured slot has one
container, and shutdown is one effect away.

This is a positive observation about Tina's model. The build is
documentation, not new product work — call it out in the user
guide's lifecycle chapter as a contrast with the Tokio shape.

### 13. Tina-owned database client (`tina-sqlx-bridge`)

**Surfaced by:** `specimen_sqlite_counter`.

There is no native or bridged path for "Tina service talks to a
database" today. The honest first-form shape used in the specimen
is one isolate that owns a `rusqlite::Connection` and runs each
query inline in `handle`. SQLite operations are fast, so this works
for a single-shard adoption-grade example, but it blocks the shard
thread for the duration of every query. For a remote DB
(Postgres) where queries take milliseconds, the same shape would
violate the bounded-handler-turn contract that makes Tina's other
patterns honest.

**Build:** `tina-sqlx-bridge` (or first-form `tina-rusqlite-bridge`
for the sync path) shaped like `tina-reqwest-bridge`:

- Tokio-owned blocking-pool runtime for the actual rusqlite/SQLx
  calls;
- bounded ingress (`mailbox_capacity`);
- typed `SqliteError::*` variants with `Closed` / `Busy` / `IoError`
  / `Decode` / `Constraint` / `Internal` shapes;
- visible `Full` / `Closed` / `Timeout`;
- metrics handle comparable to `ReqwestMetricsHandle`.

ROADMAP phase 063 names this work; this finding is the per-specimen
witness.

### 14. Spawn API surfaces the child's address

**Surfaced by:** `specimen_dynamic_worker_pool`,
`specimen_supervised_worker`.

`spawn(ChildDefinition::new(...))` returns nothing. The parent does
not learn the child's `Address`. Today this is OK because the
parent only needs the child to send messages *back* to the parent
(the child has the parent's address). But it means the parent
cannot:

- ask the runtime "is this specific child still alive?" via
  `observe_isolate_complete(child_addr)`;
- send the child a follow-up message;
- aggregate "missing partials" as a typed timeout (the parent
  doesn't know which child is missing).

Today's workaround is the supervised-worker pattern: the child
sends a `Boot(self_addr)` message back to a shared `Arc<Mutex<...>>`
slot. That works for the supervisor case, but it's the wrong
ergonomics for "spawn N workers and join all results."

**Build:** either `spawn(...)` returns the child's `Address` (would
require a synchronous spawn API today), or a
`spawn_observed(child).reply(MyMsg::ChildSpawned)` variant that
delivers `Address<ChildMsg>` to the parent as a continuation
message — analogous to `send_observed` and `runtime calls`. This
also enables a future `JoinSet`-equivalent isolate primitive.

A *host-side* alternative —
`runtime.observe_child_started::<M>(parent).wait(timeout)?` —
was considered and rejected for now: the existing
`RuntimeEventKind::Spawned { child_isolate }` event has no
`TypeId` for the child's `Message`, so a typed waiter would
either need a new field on `Spawned` (a runtime-event change)
or a caller-asserted `M` (not honest under the LLM rule). Pick
the typed-event vs. continuation form when the supervisor/spawn
API gets revisited.

### 16. Multi-worker TLS lane (or split accept/stream lanes)

**Surfaced by:** `specimen_native_https`, `tina-http/tests/client_tls_smoke.rs`.

The runtime's TLS lane is one worker thread per shard. The worker
processes one TLS op at a time: a `tls_accept` poll (busy-waiting
on the listening socket plus driving the TLS handshake) blocks
every other TLS op on that lane — `tls_read`, `tls_write`,
`tls_close`, and any concurrent `tls_connect`. Two consequences
visible from the example:

- `HttpsListener` must use a *short* `tls_accept_timeout` (default
  250ms) so the worker yields between accept polls and live
  connections can drain. Each connection's per-op latency
  effectively includes one accept-slice.
- A Tina HTTPS server and a Tina HTTPS client cannot share a
  runtime: both sides of one TLS handshake need the worker
  concurrently and they deadlock. The example puts the
  counterparty on a raw OS thread; the integration tests do the
  same. Outbound HTTPS calls work, inbound HTTPS works, but they
  must live in separate processes (or separate shards once shards
  carry independent TLS lanes).

**Build:** either a worker pool inside the existing TLS lane, or
split accept/handshake from per-stream read/write/close so each
lane has one worker (accept worker keeps poll-looping; stream
workers move data). The choice determines how throughput scales
and whether `tls_accept_timeout` can grow back to "wait
indefinitely". Keep DER-only inputs, no system roots, no HTTP/2
in scope.

**Revisit when:** real users hit the same-process server+client
constraint, or the example acquires a third role (proxy / mTLS).
Until then, first-form HTTPS is honest about its single-lane
bottleneck.

### 15. Deadline as first-class context

**Surfaced by:** `specimen_backpressure_chain`.

A multi-hop chain has to thread a deadline (or a remaining-budget
duration) through every call. Today this is `Duration` in the
request struct + a matching `IsolateCall` timeout, and the outer
hop's call timeout must be slightly longer than the inner's so the
typed downstream timeout reaches the caller before the outer
times out. With N hops, the slack accumulates and there is no
helper that names the "outer = innermost + slack" pattern.

**Decision:** do not freeze a wall-clock `Deadline` API here.
Deadline is really a clock-truth problem. A live-only helper could
exist later, but it must say it has no simulator/replay claim. A
replayable deadline should wait for the runtime/simulator clock
model, likely in the DST/replay usability work.

**Revisit when:** the clock source is explicit. Then a small
`Deadline` value can produce `.remaining() -> Duration` for
existing call APIs without adding hidden cancellation or retry.

## Closed

Findings shipped by recent phases. Numbers are kept stable so
existing README references stay valid.

### 17. Host-thread `call_blocking` — Phase 068 follow-up

Surfaced by `specimen_native_https` and native HTTP/TLS tests.
`ThreadedRuntime::call_blocking(addr, msg, timeout)` now performs
the ordinary typed Tina call through a temporary driver isolate and
returns `CallOutcome<R>` to the host thread. The HTTPS specimen and
the direct TLS client/server tests use it; tests that intentionally
need a concurrent in-flight call still keep an explicit driver.

### 18. Trace query helpers — Phase 068 follow-up

Surfaced by TLS regression tests that repeatedly scanned for
`RuntimeEventKind::CallCompleted` / `CallFailed` by hand.
`RuntimeTraceExt` now adds `count_completed`, `any_completed`,
`count_failed`, `any_failed`, `count_failed_with`, and
`count_completion_rejected` on trace slices. The helpers summarize
existing trace facts only; they do not infer hidden causality.

### 1. `observe_result` on `ThreadedMultiShardRuntime` — Phase 062 Rock 1

Surfaced by `specimen_sharded_fanout_read`, `specimen_sharded_keyspace`.
`runtime.observe_result::<Report, _, _>(addr)` now exists on the
multi-shard threaded shell with the same single-claim semantics as
the single-shard form. Both 053 specimens use it directly; the
`Arc<Mutex<Option<Report>>>` polling is gone.

### 4. Synchronous `try_send_outcome` — Phase 062 Rocks 3 & 4

Surfaced by `specimen_rate_limited_worker`,
`specimen_hot_key_fairness`. `runtime.try_send_outcome(addr, msg,
&outcomes)` plus a shared `HostBurstOutcomes` accumulator removes
the per-send observer closure, the Arc-cloned counters, and the
manual observed barrier. `runtime.send_observed_until(addr,
deadline, backoff, || msg)` covers the "control message through a
saturated mailbox" pattern with a typed
`SendObservedUntilError::{Timeout, Closed, WorkerStopped}`.

Per-send precision still rides on the worker-thread observer: true
synchronous-in-the-host mailbox inspection would violate SPSC and
expose the worker's address->mailbox registry to the host thread,
so the helper removes bookkeeping, not the worker roundtrip.

### 5. Single-in-flight gate for timer-driven workers — Phase 062 Rock 5

Surfaced by `specimen_rate_limited_worker`,
`specimen_hot_key_fairness`, and reinforced by
`specimen_periodic_batcher` / `specimen_graceful_drain_server`.
`tina_runtime::SingleCallGate` names the "at most one timer/call in
flight, plus N queued" invariant. `submit()` returns `true` when
the caller should schedule; `complete()` returns `true` when more
work is queued and the next timer should be scheduled. The gate is
plain data — it does not own the timer or the trace; the caller
still writes `sleep(...).reply(...)` so every event is visible.

### 6. Bridge call retry classifier — Phase 062 Rock 6

Surfaced by `specimen_retrying_outbound_http`,
`specimen_webhook_fanout`. `ReqwestOutcomeExt::classify` returns
`ReqwestOutcomeClass::{Succeeded, Transient(reason),
Fatal(reason)}` with typed reason payloads. The raw layered
`ReqwestCallOutcome` and `flatten_outcome` are unchanged; the
classifier is opt-in sugar. `specimen_retrying_outbound_http` and
`specimen_webhook_fanout` now match three arms instead of six.

### 10. Retry helper at the service edge — Phase 062 Rock 4

Closed by the same Rock as finding 4. `send_observed_until` covers
both shapes — burst-message ingress and one-shot control-message
delivery through a saturated mailbox.

## How To Add A Finding

Only add to this file when the finding implies Tina product work.

```md
### N. Short product-shaped title

**Surfaced by:** `example_name`, `other_example`.

What repeated pain we saw.

**Build:** concrete primitive, API, doc, or test work.
```

Per-example flavor belongs in the example README. Resolved
archaeology belongs in `FINDINGS_HISTORY.md`.

Numbers are stable: when a finding closes, move it down to
[Closed](#closed) and keep its number so external references
(README links, commit messages, prior PRs) stay valid.

## Resolved Or Retired Round 1 (Phase 053 + 059)

Round 1 closed in Phase 059 + Phase 053. Those nine items are
archived verbatim in [`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md).
Short summary of patterns no new code should copy:

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
  value: use `stop_with(value)` +
  `runtime.observe_result::<T>(addr)?` (works on single-shard and
  multi-shard threaded runtimes; see active finding 1's closure
  above);
- per-comparison shard types: use `SingleShard` for one-shard programs and
  `tina_runtime::sharded::ShardPlacement` / `ShardServiceTable` for
  multi-shard placement.
