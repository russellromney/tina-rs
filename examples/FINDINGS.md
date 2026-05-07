# Eiffel Findings — Round 2

This file is the current action list. Eiffel examples are specimens: they
show how Tokio and Tina code feel for the same kind of job. When the same
Tina pain appears across specimens, it becomes runtime/API work here.

Round 1 closed in Phase 059 + Phase 053. Those nine items are archived
verbatim in [`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md); they should not be
re-opened in this file. Round 2 starts from what the post-053 specimens
surface.

## Round 2 product improvements

### 1. `observe_result` on `ThreadedMultiShardRuntime` — closed by Phase 062 Rock 1

**Surfaced by:** `eiffel_sharded_fanout_read`, `eiffel_sharded_keyspace`.

Phase 062 Rock 1 lifted `observe_result` to `ThreadedMultiShardRuntime`.
The registration is routed to the address's owning shard the same way
`try_send` is routed today; the surface, error vocabulary
(`ResultWaitError`), and single-claim semantics match
`ThreadedRuntime::observe_result`.

Both 053 specimens now use it directly:
`stop_with(report)` inside the coord/driver, `runtime
.observe_result::<Report, _, _>(addr)?.wait(deadline)` on the host —
no `Arc<Mutex<Option<Report>>>` polling.

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

### 4. Synchronous `try_send_outcome` — closed by Phase 062 Rocks 3 & 4

**Surfaced by:** `eiffel_rate_limited_worker`.

Phase 062 Rock 3 ships `runtime.try_send_outcome(addr, msg, &outcomes)`
plus a shared `HostBurstOutcomes` accumulator. The accumulator wraps
the existing `try_send_and_observe_with` shape; what it removes is the
per-send closure, the Arc-cloned counters, and the manual observed
barrier the caller used to spell out by hand. Every truth-typed outcome
stays distinct in the snapshot (`admitted`, `mailbox_full`,
`mailbox_closed`, `ingress_full`, `worker_stopped`).

The Round 2 design note recorded in `.intent/phases/062.../plan.md`:
true synchronous-in-the-host mailbox inspection would violate SPSC and
expose the worker's address->mailbox registry to the host thread, so
per-send precision still rides on the worker-thread observer. The
helper removes bookkeeping, not the worker roundtrip.

Phase 062 Rock 4 ships `runtime.send_observed_until(addr, deadline,
backoff, || msg)` for "control" messages like `BurstClosed(n)` that
travel through the same bounded data mailbox as work items. It retries
on `MailboxFull` / `IngressFull` until the deadline and returns typed
`SendObservedUntilError::{Timeout, Closed, WorkerStopped}` — no hidden
queue, no second mailbox. `eiffel_rate_limited_worker` now uses both
helpers; the hand-rolled observer-closure burst loop and the
`std::thread::sleep` retry loop are both gone.

### 5. Single-in-flight gate for timer-driven workers — closed by Phase 062 Rock 5

**Surfaced by:** `eiffel_rate_limited_worker`.

Phase 062 Rock 5 ships `tina_runtime::SingleCallGate` — a tiny stateful
helper that names the "at most one timer/call in flight, plus N
queued" invariant. `submit()` returns `true` when the caller should
schedule the timer/call (gate was idle); `false` while a previous one
is still racing. `complete()` returns `true` when more work is queued
and the next timer should be scheduled. The gate is plain data — it
does not own the timer, the trace, or the message; the caller still
writes `sleep(...).reply(...)` itself, so every `Sleep` event still
appears in the trace.

`eiffel_rate_limited_worker` now uses the gate; the hand-rolled
`pending: u32` / `was_idle = pending == 0` lines are gone.

### 6. Bridge call retry classifier — closed by Phase 062 Rock 6

**Surfaced by:** `eiffel_retrying_outbound_http`.

Phase 062 Rock 6 ships `ReqwestOutcomeExt::classify` returning
`ReqwestOutcomeClass::{Succeeded, Transient(reason), Fatal(reason)}`
with typed reason payloads
(`UpstreamServer { status }`, `UpstreamClient { status }`,
`BridgeTimeout`, `WorkerTimeout`, `WorkerTransport`, `BridgeFull`,
`BridgeClosed`, `WorkerFull`, `WorkerClosed`, `RequestTooLarge`,
`ResponseTooLarge`, `InvalidRequest`). The raw layered
`ReqwestCallOutcome` and the `flatten_outcome` helper are unchanged —
the classifier is opt-in sugar.

`eiffel_retrying_outbound_http` now matches three arms instead of six.

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
