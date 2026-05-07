# Phase 061: Bounded Deferred Replies And Service Fanout

## Goal

Teach Tina to remember many caller promises at once without becoming soup.

059 found the hard runtime-model gap under pooled services, fanout, bridge
workers, and sharded frontends:

```text
one frontend receives many calls
frontend forwards work to many workers
worker replies arrive later, out of order
each original caller must get the right reply
```

Today `Effect::Reply` answers the active message's current call context. That
is great for one-at-a-time service code. It is not enough for a frontend that
must hold N original caller promises while N worker calls are in flight.

Near-grug:

> Tina can remember many promises. But promises live in a named box. Box has a
> cap. Late/full/closed/timeout show in trace.

## Baseline

Already exists:

- `call(service, req, timeout).reply(MyMsg::Done)` with timeout and
  `CallOutcome` truth;
- runtime-call continuation context through `.reply(...)` chains;
- `Effect::Reply` for the current caller;
- visible `CallReplyRejected` when late isolate-call replies no longer match a
  pending caller;
- bounded mailboxes and bounded cross-shard reply paths;
- `MailboxBudget` and pressure summaries from 059;
- RPC single-service topology from 052/058;
- 053 sharded service plan waiting on a good fanout/frontend primitive;
- bridge crates that need multi-caller request correlation without unbounded
  pending maps.

Missing:

- no first-class way to capture the current caller promise and reply later;
- no bounded pending-promise box helper;
- no caller-timeout/caller-closed signal for held promises;
- no shared fanout/gather helper;
- no service-pool proof that keeps original caller replies correct under
  out-of-order worker completions;
- no simulator proof for deferred reply slots.

## Non-Goals

- No async handlers.
- No hidden futures.
- No hidden unbounded pending map.
- No hidden retry.
- No automatic queue behind `Full`.
- No full RPC topology framework in this phase.
- No gRPC, HTTP/2, AWS, SQL, or bridge-specific production wrapper.
- No "make every service pool magic" macro.
- No broad performance claim.

## Rules

- Many caller promises require a named cap.
- Capturing a promise is explicit.
- A captured promise is one-shot.
- A dropped promise is visible.
- A timed-out caller closes the promise.
- A late worker reply is visible and does not panic.
- Full pending state returns `Full` instead of buffering.
- Helpers may reduce correlation boilerplate; they may not hide capacity,
  timeout, partial failure, retry, or topology.
- Sim and live must agree on the externally visible meaning.

## Bad Magic

This phase must not add:

- no `HashMap` of caller promises without a cap;
- no retry hidden inside a helper;
- no queued work after `Full`;
- no promise that can be replied twice;
- no promise that keeps capacity after the caller left;
- no "probably delivered" trace;
- no async handler.

## Design Alternatives

### A. Raw Deferred Reply Slot

Expose the core capability directly:

```rust
let slot: DeferredReply<Response> = ctx.take_reply_slot()?;
self.pending.insert(id, slot)?;

// later, from a worker completion message:
reply_to(slot, value)
```

Pros:

- most general;
- unlocks service pools, fanout/gather, sharded frontends, and bridge workers;
- makes the real model visible.

Cons:

- sharp user API;
- easy to build weird flow if used directly everywhere;
- needs strong one-shot/drop/timeout trace rules.

This is likely the right runtime foundation.

### B. Bounded Pending Replies Helper

Expose a small container around reply slots:

```rust
self.pending.try_capture(id, ctx)?;
call(worker, req, timeout).reply(|outcome| Msg::WorkerDone(id, outcome))

// later:
self.pending.reply(id, result)
```

Pros:

- safer default surface;
- capacity and correlation live in one named object;
- common service code avoids hand-written `HashMap` mistakes.

Cons:

- not every fanout shape fits;
- raw primitive still likely needed underneath.

This is likely the right everyday API.

### C. New Effect Variant

Keep runtime delivery as an effect:

```rust
reply_to(slot, value)
```

desugars to something like:

```rust
Effect::ReplyTo(slot, value)
```

Pros:

- matches Tina's "handler returns data, runtime performs effect" model;
- sim/live tracing can be uniform;
- delivery remains runtime-owned.

Cons:

- expands the closed `Effect` enum;
- touches `tina`, `tina-runtime`, and `tina-sim`;
- needs careful erasure and typed reply handling.

This is probably necessary if deferred replies are a real runtime verb.

### D. Service Pool Primitive First

Hide reply slots behind one product API:

```rust
PooledService::new(workers, config)
```

Pros:

- solves the immediate 058/059 RPC topology pain;
- smaller surface for users.

Cons:

- too narrow for HTTP pools, sharded maps, bridge workers, scatter/gather;
- other domains will reinvent the primitive.

Use this as proof, not as the only foundation.

### E. Call Correlator Sugar

Add a token to outgoing calls:

```rust
call(worker, req, timeout)
    .correlate(id)
    .reply(Msg::WorkerDone)
```

Pros:

- reduces mismatched reply boilerplate;
- makes out-of-order worker completions easier to read.

Cons:

- does not hold the original caller promise;
- only solves half the pool problem.

Good optional sugar after the core slot/helper shape is proved.

## Chosen First Shape

061 should be layered:

1. API home decision;
2. raw typed one-shot deferred reply slot;
3. effect helper to reply through the slot;
4. bounded pending-promise box helper;
5. one pooled-service proof;
6. one small fanout/gather proof;
7. simulator parity over full/closed/timeout/late cases.

If the raw primitive feels too sharp, keep it public but documented as the
advanced escape hatch. Make the bounded helper the blessed user path.

## Rocks

1. **API Home**

   Decide where the primitive lives before helper code grows roots.

   First answer:

   - core deferred reply types live in `tina_runtime::deferred`;
   - runtime trace facts live with the existing runtime event model;
   - simulator mirrors the same semantic facts;
   - `tina-rpc`, `tina-http`, and bridge crates may build adapters on top;
   - examples may not invent their own reusable promise registry.

   Why:

   - this is a runtime-model primitive, not an RPC feature;
   - service pools, fanout, sharded frontends, and bridge workers all need it;
   - putting it in `tina-rpc` would force every other domain to rebuild it.

   Near-grug:

   > promise box belongs near runtime. RPC merely uses box.

2. **Deferred Reply Slot Core**

   Add a first-class one-shot reply capability.

   Desired shape:

   ```rust
   let slot: DeferredReply<Response> = ctx.take_reply_slot()?;
   ...
   reply_to(slot, value)
   ```

   Requirements:

   - capture is only legal while handling a call message;
   - capture fails visibly when the current message has no caller;
   - slot is typed as `DeferredReply<R>`;
   - wrong reply type does not compile at the public API;
   - slot is one-shot;
   - duplicate reply attempt returns a typed rejection or records a trace event;
   - dropping a live slot is visible;
   - caller timeout/closed state closes the slot;
   - no replay cache;
   - no hidden queue.

   Open design point:

   - whether `reply_to` is a new `Effect` variant or a runtime-call-like
     effect family. Preferred first answer: new `Effect::ReplyTo` because the
     runtime owns delivery.
   - internal runtime erasure may still need a defensive mismatch event, but
     `TypeMismatch` must not be part of the normal user-facing contract.

3. **Trace Vocabulary**

   Add trace events for promise lifecycle.

   Candidate facts:

   ```text
   DeferredReplyCaptured
   DeferredReplySent
   DeferredReplyRejected { reason: CallerClosed | ReplyPathFull | RequesterShardClosed }
   DeferredReplyDropped
   ```

   Exact names may change. The facts may not.

   Requirements:

   - every captured slot has a terminal trace fact;
   - late worker completion after caller timeout is visible;
   - service stop with pending slots is visible;
   - duplicate reply does not silently disappear;
   - trace remains deterministic under simulator replay.

4. **Bounded Pending Replies Helper**

   Build the grug container most users should hold:

   ```rust
   PendingReplies<K, R>::with_capacity(64)
   ```

   Requirements:

   - insert/capture or `Full`;
   - remove on successful reply;
   - remove on caller timeout/closed;
   - drain on service stop;
   - duplicate key is visible;
   - duplicate reply is visible;
   - metrics/debug: current, high-water, full count, dropped count;
   - bounded storage, not merely a normal growing `HashMap` with a limit check;
   - first form should use an explicit fixed-capacity table/slab/ring shape or
     another implementation that cannot grow beyond `capacity`;
   - abandoned or closed slots must be pruned before capacity checks;
   - key type must be explicit and owned.

   Near-grug:

   > mailbox holds messages. pending box holds promises. both need caps.

5. **Caller Cancellation And Reclaim**

   Let a service holding promises learn when a caller left.

   First-form ownership rule:

   - the runtime owns caller liveness truth;
   - `DeferredReply<R>` can answer "still open?" without racing user state;
   - `PendingReplies` owns reclaim for slots it holds;
   - reclaim happens before admission capacity checks;
   - service stop drains remaining slots and emits terminal facts.

   Candidate surface:

   ```rust
   self.pending.sweep_closed(ctx)?;
   ctx.reply_slot_status(slot)
   observe_deferred_reply_closed(...)
   ```

   Requirements:

   - a caller timeout eventually closes the slot;
   - service can stop wasting work when it notices;
   - cleanup does not require polling an unbounded list;
   - bridge workers can drop local pending state for cancelled callers;
   - live and sim agree.

   This does not require preempting already-started external work. It only
   makes the lost caller promise visible and reclaimable.

   Required nasty test:

   ```text
   fill pending box
   callers time out or drop
   pending box reclaims slots
   new callers can enter without stale Full
   ```

6. **Pooled Service Frontend Proof**

   Build one real pool frontend on top of the primitive.

   Shape:

   ```text
   caller -> frontend -> one of N workers
   worker replies out of order
   frontend replies to the matching original caller
   ```

   Requirements:

   - fixed worker list;
   - explicit `PoolConfig { workers, max_pending, worker_call_timeout }`;
   - no hidden retry;
   - `Full` when pending promises are full;
   - `Closed` when frontend is shutting down;
   - worker timeout maps visibly;
   - stopped worker maps visibly;
   - out-of-order completions route correctly.

   This proof may live in `tina-rpc` if it is RPC-shaped. The deferred reply
   primitive and pending box may not live there.

7. **Fanout/Gather Helper**

   Build the smallest bounded scatter/gather helper that reuses deferred
   replies.

   Shape:

   ```rust
   FanoutConfig {
       max_targets,
       max_in_flight,
       aggregate_timeout,
       collector_mailbox_capacity,
   }
   ```

   Requirements:

   - bounded target list;
   - bounded result collection;
   - partial aggregate result;
   - aggregate timeout visible;
   - per-target `Full`/`Closed`/`Timeout` visible;
   - first form is all-targets with bounded partial result;
   - no automatic retry.

   Quorum and first-success are later policy work unless an Eiffel specimen
   immediately proves they are needed.

   This should align with 053 sharded service primitives.

8. **Bridge Worker Proof**

   Add one fake bridge-shaped proof.

   Shape:

   ```text
   many Tina callers -> bridge worker -> fake external async completions
   completions arrive out of order
   cancelled caller does not leak slot
   ```

   Requirements:

   - no real reqwest/sqlx/AWS dependency needed;
   - proves the bridge pattern, not HTTP;
   - bounded admission;
   - caller timeout before external completion;
   - external completion before caller timeout;
   - external completion after service stop;
   - current/high-water metrics visible.

9. **Simulator Parity**

   Add simulator support and saved-seed tests.

   Scenarios:

   - reply before timeout;
   - timeout before reply;
   - caller stops before reply;
   - frontend stops with pending promises;
   - pending box full;
   - duplicate reply attempt;
   - worker replies out of order.

   Required proof:

   - same seed, same trace fingerprint;
   - different seed can change completion order;
   - live-vs-sim projection matches the semantic facts.

10. **Docs And Examples**

   Update the user guide and one or two Eiffel specimens.

   Docs must show:

   - when to use plain `call(...).reply(...)`;
   - when to capture a deferred reply;
   - why the pending box needs a cap;
   - mailbox budget vs pending-promise budget;
   - how timeout/closed/late appear in trace;
   - anti-pattern: `Arc<Mutex<HashMap<RequestId, oneshot>>>` with no cap.

   Candidate specimens:

   - `eiffel_rpc` pooled service;
   - `eiffel_mini_keyspace` sharded/fanout read;
   - a tiny new `eiffel_service_pool` if existing examples are too noisy.

## Suggested Order

1. API home decision.
2. Trace vocabulary and semantic contract.
3. Raw typed deferred reply slot.
4. `reply_to` effect/helper.
5. Bounded `PendingReplies`.
6. Focused runtime tests for full/closed/timeout/drop/duplicate/reclaim.
7. Simulator support.
8. Pooled service proof.
9. Small fanout/gather proof.
10. Docs and Eiffel specimen rewrite.

Reasoning:

- API home before code prevents helper sprawl;
- trace facts before behavior keep the feature honest;
- raw primitive before helper avoids building a helper on sand;
- helper before examples prevents hand-rolled maps;
- sim before broad examples catches model drift early;
- pooled service is the practical proof that 059 Rock 8 was blocked for a real
  reason and is now unblocked.

## Required Proof

- `cargo test -p tina-runtime` covers raw deferred reply semantics.
- `cargo test -p tina-sim` covers deterministic replay and saved-seed cases.
- One pool/frontend test routes out-of-order replies to the correct callers.
- One fanout/gather test returns a bounded partial aggregate.
- One cancellation test proves timed-out callers do not leak pending slots.
- One reclaim test fills pending, closes callers, and admits new callers
  without stale `Full`.
- One service-stop test drains/drops pending promises visibly.
- One pressure test proves pending full returns `Full`, not hidden buffering.
- Clippy/fmt/doc tests stay green.

## Done Means

- A frontend isolate can safely hold many original caller promises.
- The number of promises is named and capped.
- Pools and fanout no longer need ad hoc unbounded maps.
- Caller timeout/closed/late reply are trace-visible.
- Pooled RPC/service topology becomes expressible.
- 053 sharded service primitives have a real fanout foundation.
- Bridge crates have a pattern for many concurrent external requests without
  losing Tina pressure truth.
