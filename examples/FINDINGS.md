# Eiffel Findings — Current Product Work

This file is the current action list. Eiffel examples are specimens: they
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

**Surfaced by:** `eiffel_sharded_fanout_read`,
`eiffel_dynamic_worker_pool`.

The `ReplyAdapter` pattern needs the coord's own address to wire the
adapter, and the coord needs the adapter's address before it can fan out.
Today the answer is a `Bind { bridge }` (or `Begin { self_addr }`) message
before `Start`. That works but adds a variant whose only job is to land
"you, isolate, look here for your replies" into the isolate's state.

Phase 062 Rock 2 was designed for this (`register_with_capacity_using<I, E>`
with a `|self_addr| ...` constructor closure) but did not ship in the
five-Rock landing.

**Build:** a way for an isolate to learn its own typed address at register
time — for example, a constructor closure parameter `|self_addr| {
ScatterCoord { ..., self_addr } }`. Avoids the bind-before-start handshake
and removes the `Option<Address<...>>` field that's only `None` for one
turn.

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

### 8. External cancellation API

**Surfaced by:** `eiffel_cancellation_chain`.

There is no public `runtime.cancel(addr)` and no public
`IsolateCall::abort()`. The only way to "externally cancel" mid-
flight work today is to send a domain `Stop` message to the
requester isolate, which causes it to stop itself. Stopping the
requester closes its pending IsolateCalls and any worker reply
that arrives later is rejected as `CallReplyRejected
{ RequesterClosed }`. That works, but every isolate that wants to
be externally cancellable has to add its own `Stop` (or
equivalent) variant.

**Build:** a runtime-level `runtime.cancel(addr) -> CancelOutcome`
that closes pending IsolateCalls owned by `addr` without
requiring user-defined cancellation messages. Or a typed
`IsolateCall::abort(handle)` that the requester can stash and use
to drop a single in-flight call without stopping itself.

### 9. Drain helper for `PendingReplies` at service stop

**Surfaced by:** `eiffel_graceful_pool_shutdown`.

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

**Build:** `pending.drain_into_effect(R::Closed) -> Effect<I>` (or
similarly named) that returns the matching `Effect::Batch` in one
call, with the trailing `stop()` opt-in via a sibling
`drain_into_stop_effect(R::Closed)`. Same lifecycle truth, less
boilerplate.

### 11. Multi-stage pipeline ergonomics

**Surfaced by:** `eiffel_two_stage_pipeline`.

A 3-stage pipeline reads as 4 enum variants in `PipelineMsg`
(Submit + Parsed + Validated + Executed), each with its own match
arm. The Tokio side reads as `parse(i).await?; validate(p).await?;
execute(v).await?` — three lines. The Tina version is correct and
trace-visible at every stage, but the variant count grows
linearly with stage count.

**Build:** a pipeline-shaped helper that takes a `[StageAddr; N]`
and the captured deferred reply slot, walking through stages with
a single continuation message. Must preserve per-stage timeout
truth and the typed bail-out arms; this is sugar, not a hidden
state machine. (`SingleCallGate` does not apply here — it solves
single-in-flight timer gating, not stage chaining.)

### 12. Rust footgun replication: shared receiver in worker pool

**Surfaced by:** `eiffel_graceful_pool_shutdown` (Tokio side).

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

## Closed

Findings shipped by recent phases. Numbers are kept stable so
existing README references stay valid.

### 1. `observe_result` on `ThreadedMultiShardRuntime` — Phase 062 Rock 1

Surfaced by `eiffel_sharded_fanout_read`, `eiffel_sharded_keyspace`.
`runtime.observe_result::<Report, _, _>(addr)` now exists on the
multi-shard threaded shell with the same single-claim semantics as
the single-shard form. Both 053 specimens use it directly; the
`Arc<Mutex<Option<Report>>>` polling is gone.

### 4. Synchronous `try_send_outcome` — Phase 062 Rocks 3 & 4

Surfaced by `eiffel_rate_limited_worker`,
`eiffel_hot_key_fairness`. `runtime.try_send_outcome(addr, msg,
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

Surfaced by `eiffel_rate_limited_worker`,
`eiffel_hot_key_fairness`. `tina_runtime::SingleCallGate` names the
"at most one timer/call in flight, plus N queued" invariant.
`submit()` returns `true` when the caller should schedule;
`complete()` returns `true` when more work is queued and the next
timer should be scheduled. The gate is plain data — it does not
own the timer or the trace; the caller still writes
`sleep(...).reply(...)` so every event is visible.

### 6. Bridge call retry classifier — Phase 062 Rock 6

Surfaced by `eiffel_retrying_outbound_http`,
`eiffel_webhook_fanout`. `ReqwestOutcomeExt::classify` returns
`ReqwestOutcomeClass::{Succeeded, Transient(reason),
Fatal(reason)}` with typed reason payloads. The raw layered
`ReqwestCallOutcome` and `flatten_outcome` are unchanged; the
classifier is opt-in sugar. `eiffel_retrying_outbound_http` and
`eiffel_webhook_fanout` now match three arms instead of six.

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
