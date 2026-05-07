# Eiffel Findings — Round 2

This file is the current action list. Eiffel examples are specimens: they
show how Tokio and Tina code feel for the same kind of job. When the same
Tina pain appears across specimens, it becomes runtime/API work here.

Round 1 closed in Phase 059 + Phase 053. Those nine items are archived
verbatim in [`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md); they should not be
re-opened in this file. Round 2 starts from what the post-053 specimens
surface.

## Round 2 product improvements

### 1. `observe_result` on `ThreadedMultiShardRuntime`

**Surfaced by:** `eiffel_sharded_fanout_read`, `eiffel_sharded_keyspace`.

`ThreadedRuntime::observe_result::<T, _, _>(addr)?` is the blessed Phase 059
Rock 1 way to read an isolate's typed final value. It is shipped on the
single-shard threaded runtime but not on `ThreadedMultiShardRuntime`, so
multi-shard examples still fall back to `Arc<Mutex<Option<Report>>>`
polling for the final value. Both 053 examples now do this dance.

**Build:** lift `observe_result` to `ThreadedMultiShardRuntime`. The
underlying `Runtime::observe_result` already exists; the multi-shard
threaded shell just needs to route the registration call to the address's
owning shard the same way `register_with_capacity_on` does today. Same
contract as the single-shard form.

### 2. ScatterCoord setup is heavy for the happy path

**Surfaced by:** `eiffel_sharded_fanout_read`.

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

**Surfaced by:** `eiffel_sharded_fanout_read`.

The `ReplyAdapter` pattern needs the coord's own address to wire the
adapter, and the coord needs the adapter's address before it can fan out.
Today the answer is a `Bind { bridge }` message before `Start`. That works
but adds a variant whose only job is to land "you, isolate, look here for
your replies" into the isolate's state.

**Build:** a way for an isolate to learn its own typed address at register
time — for example, a constructor closure parameter `|self_addr| {
ScatterCoord { ..., self_addr } }`. Avoids the bind-before-start handshake
and removes the `Option<Address<...>>` field that's only `None` for one
turn.

### 4. Synchronous `try_send_outcome`

**Surfaced by:** `eiffel_rate_limited_worker`.

The threaded runtime offers three send shapes today:

- `try_send` — fire-and-forget; only surfaces `IngressFull` (command queue
  full), never `MailboxFull`;
- `send_and_observe` — synchronous; distinguishes `MailboxFull` from
  `IngressFull` / `Closed`, but each call is a worker-thread roundtrip, so
  a tight burst from the host is gated by worker step rate. The mailbox
  never fills, so overload is never visible at the producer;
- `try_send_and_observe_with` — non-blocking; takes an observer closure
  that fires on the worker thread later. Visible overload, but the
  call-site shape is heavy (one closure per send, atomics for accounting,
  manual barrier wait until every observer has fired).

For the "host bursts N messages, wants to know per-send whether the
mailbox accepted" pattern, today the answer is `try_send_and_observe_with`
plus a hand-rolled accounting loop. The natural shape would be a
synchronous-but-precise `try_send_outcome(addr, msg) -> Result<(),
SendOutcomeError>` that returns the same `MailboxFull` / `IngressFull` /
`Closed` typed error as `send_and_observe` *without* the per-call worker
roundtrip — by checking the mailbox synchronously in the host before
queueing the command.

**Build:** a synchronous outcome-typed try_send. May reuse the same
`ThreadedSendObservedError` enum.

The Phase 059 Rock 5 `send_blocking` / `send_retrying` plan also covers
this surface but is plan-only as of 2026-05-07; closing this finding
likely closes Rock 5 too.

### 5. Single-in-flight gate for timer-driven workers

**Surfaced by:** `eiffel_rate_limited_worker`.

A worker isolate that uses `sleep(window).reply(Tick)` to rate-limit its
processing must never have more than one timer in flight, or the rate
limit collapses (every Submit kicks off its own sleep). The current shape
is a `pending: u32` counter and a `was_idle = pending == 0` check inside
the handler. That's correct but it's the same five lines wherever this
shape appears.

**Build:** a small "single-call gate" helper for isolate state. Could be a
`SingleCallGate<R>` field that returns either an effect (when idle) or
records a deferred entry (when busy). On the runtime side, this might be a
trait `IsolateCallGate` that picks the next deferred entry on completion.
Should not hide trace truth: every `sleep` is still one trace event.

### 6. Bridge call retry classifier

**Surfaced by:** `eiffel_retrying_outbound_http`.

A caller-owned retry loop against the reqwest bridge has to write a six-arm
match against `ReqwestCallOutcome` to classify "is this transient?":
`Replied(Ok(resp))` (check `resp.status.is_server_error()`),
`Replied(Err(ReqwestError::Timeout | Reqwest(_)))` (transient),
`Replied(Err(_other))` (fatal), `Timeout` (transient), `Full | Closed`
(fatal). Most apps want the same three buckets: succeeded / transient /
fatal.

**Build:** a small classifier helper on `ReqwestCallOutcome`:

```rust
match outcome.classify() {
    OutcomeClass::Succeeded(resp)         => ...,
    OutcomeClass::Transient(reason)       => retry,
    OutcomeClass::Fatal(reason)           => fail,
}
```

Where the per-bucket `reason` names which sub-cause hit (`UpstreamServer
{ status }`, `BridgeTimeout`, `WorkerTimeout`, `Reqwest`, etc.). The
typed multi-arm match still works — this is opt-in sugar.

This is a smaller version of "Tina-shaped retry sugar" — not a hidden
retry helper, just a classifier so caller-owned retry loops are five
lines instead of fifteen.

### 7. Reqwest-bridge flatten edge: useful but per-call-site

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

### 8. Tina-owned database client (`tina-sqlx-bridge`)

**Surfaced by:** `eiffel_sqlite_counter`.

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

ROADMAP phase 055 names this work; this finding is the per-specimen
witness.

### 9. Self-address-aware spawn / fanout helper

**Surfaced by:** `eiffel_dynamic_worker_pool`,
`eiffel_sharded_fanout_read`.

A coordinator that wants to spawn N children that send back to it
must first be told its own `Address`. Today the pattern is a
bootstrap `Begin { self_addr: Address<CoordMsg> }` message that the
host sends after `register`. Same shape shows up in
`eiffel_sharded_fanout_read` as the `Bind { bridge }` handshake.
The variant exists only to plug the "isolate doesn't know its own
address until after registration" gap.

**Build:** a `register_with_capacity_using<F>(capacity, f)` form
where `f: FnOnce(Address<I::Message>) -> I` lets the constructor
see its own typed address, removing the `Begin/Bind` bootstrap
variant. Round 2 finding 3 named this for the scatter/gather coord;
this specimen reinforces it for plain spawn-and-join.

### 10. Spawn API that surfaces the child's address

**Surfaced by:** `eiffel_dynamic_worker_pool`.

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

### 11. Deadline as first-class context

**Surfaced by:** `eiffel_backpressure_chain`.

A multi-hop chain has to thread a deadline (or a remaining-budget
duration) through every call. Today this is `Duration` in the
request struct + a matching `IsolateCall` timeout, and the outer
hop's call timeout must be slightly longer than the inner's so the
typed downstream timeout reaches the caller before the outer
times out. With N hops, the slack accumulates and there is no
helper that names the "outer = innermost + slack" pattern.

**Build:** a small `Deadline` value type carrying `(start: Instant,
total: Duration)` with `.remaining() -> Duration`. A future
`call_with_deadline(addr, msg, deadline)` would compute the
matching `IsolateCall` timeout (slightly larger than the budget
the callee should use) automatically.

### 12. Drain timeout for isolate shutdown

**Surfaced by:** `eiffel_graceful_drain_server`.

The bridge crate (`tina-tokio-bridge::BridgeShutdownReport`) has a
`drained_within_timeout` flag for the bridge case. The same shape
applies at the isolate level: a `Drain` message that says "finish
in-flight, then stop" should accept a deadline, with the report
saying whether drain completed inside it. Today this is a
hand-rolled `DrainDeadlineFired` continuation message scheduled via
`sleep` and a check in the isolate's "is it done" predicate that
returns true on deadline-fired even when `pending > 0`.

**Build:** a small `DrainGate` helper for isolate state that holds
the deadline + the pending-count predicate, with an `is_done` /
`drained_within_timeout` accessor that the handler reuses.

### 13. Single-in-flight timer gate (reinforced)

**Surfaced by:** `eiffel_periodic_batcher` (again).

Round 2 finding 5 already named this from
`eiffel_rate_limited_worker`. The periodic batcher needs the same
pattern: a generation counter on `sleep(...).reply(Tick)` so that a
size-triggered flush can invalidate a still-pending timer's
eventual `Tick` without canceling the runtime call. The batcher's
shape is identical to the rate-limited worker's; the user-side
boilerplate is the same five lines.

The product work named in finding 5 (`SingleSleepGate` /
`SingleCallGate<R>`) covers this. Reinforcing here to keep the
"it's everywhere" signal visible.

## How To Add A Finding

Only add to this file when the finding implies Tina product work. Round 2
is for new pain that the post-053 specimens surface.

```md
### N. Short product-shaped title

**Surfaced by:** `example_name`, `other_example`.

What repeated pain we saw.

**Build:** concrete primitive, API, doc, or test work.
```

Per-example flavor belongs in the example README. Resolved archaeology
belongs in `FINDINGS_HISTORY.md`.

## Resolved Or Retired By Recent Phases

These used to be current pain and should not be copied into new code.
Round 1 list, kept short here; the long form is in
[`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md):

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
  value (single-shard): use `stop_with(value)` +
  `runtime.observe_result::<T>(addr)?`. (Multi-shard is Round 2 finding 1.)
- per-comparison shard types: use `SingleShard` for one-shard programs and
  `tina_runtime::sharded::ShardPlacement` / `ShardServiceTable` for
  multi-shard placement.
