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

**Surfaced by:** `eiffel_graceful_pool_shutdown`,
`eiffel_graceful_drain_server`.

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

### 10. `try_send`/`send_and_observe` retry helper at service edge

**Surfaced by:** `eiffel_hot_key_fairness`,
`eiffel_graceful_pool_shutdown`,
`eiffel_rate_limited_worker` (Round 2 already).

The host-side "send a control message through a bounded mailbox
that may be saturated" pattern keeps appearing:

```rust
let close_deadline = Instant::now() + Duration::from_secs(2);
loop {
    match runtime.send_and_observe(addr, msg.clone()) {
        Ok(()) => break,
        Err(MailboxFull) | Err(IngressFull) => {
            if Instant::now() >= close_deadline { return Err(...); }
            std::thread::sleep(Duration::from_millis(2));
        }
        Err(e) => return Err(...),
    }
}
```

**Build:** `runtime.send_blocking(addr, msg, deadline)` (named in
Round 2 finding 4 / Phase 059 Rock 5, still plan-only as of
2026-05-07). One sentence that covers the four lines of retry
pattern, plus a typed `Sent`/`Timeout`/`Closed` outcome.

### 11. Multi-stage pipeline ergonomics

**Surfaced by:** `eiffel_two_stage_pipeline`.

A 3-stage pipeline reads as 4 enum variants in `PipelineMsg`
(Submit + Parsed + Validated + Executed), each with its own match
arm. The Tokio side reads as `parse(i).await?; validate(p).await?;
execute(v).await?` — three lines. The Tina version is correct and
trace-visible at every stage, but the variant count grows
linearly with stage count.

**Build:** the Round 2 finding 5 work (`SingleSleepGate`) plus a
pipeline-shaped helper that takes a `[StageAddr; N]` and a slot,
walking through stages with a single continuation message. Must
preserve per-stage timeout truth and the typed bail-out arms;
this is sugar, not a hidden state machine.

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
