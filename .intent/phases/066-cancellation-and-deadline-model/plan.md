# 066 Cancellation And Deadline Model

## Status

- Done: design drafted from Eiffel cancellation/backpressure/pool findings.
- In progress: none.
- Open: implement the first cancellation primitive and deadline value.
- Deferred: external `cancel_isolate`, resource-driver cancellation expansion,
  fake cancellation of already-accepted foreign work, broad workflow macros.

## Goal

Give Tina a public cancellation/deadline model that is exact enough for real
services and simple enough for examples to copy.

Core rule:

```text
Cancellation means caller/runtime stopped waiting.
It does not mean work vanished.
```

First form cancels the wait. It does not promise to cancel the work.

This phase comes before 067 pools. Pools without honest cancellation become
waiter-leak machines.

Pool rule:

```text
Canceling a call does not release a pool lease.
Only explicit Release/Retire returns pool capacity.
```

Runtime can stop waiting. Runtime cannot know whether a borrowed resource is
safe to reuse after cancelled work.

Compiler rule:

```text
If compiler can know wrong, make wrong not compile.
If only runtime can know wrong, make typed outcome plus trace fact.
```

## Non-Goals

- No `runtime.cancel_isolate(addr)` first form.
- No "kill this worker thread".
- No fake cancellation for reqwest/sqlite/aws work already accepted by the
  foreign system.
- No hidden retry.
- No hidden queue.
- No async/await-shaped workflow sugar.
- No helper that hides `Full`, `Closed`, `Timeout`, or late reply rejection.

## Vocabulary

Cancellation causes should be concrete, not theological.

First-form names:

```rust
enum CancelCause {
    CallerCancelled,
    CallerTimedOut,
    OwnerStopped,
    CalleeStopped,
    ResourceClosed,
    RuntimeStopped,
}

enum CancelOutcome {
    Cancelled,
    AlreadyCompleted,
    AlreadyCancelled,
    CallerClosed,
    RuntimeStopped,
}
```

Exact names may change, but the distinctions must stay.

`CancelOutcome` and any cancel/release result types should be `#[must_use]`.
Ignoring cancellation truth should be noisy.

Trace facts must distinguish:

- caller timeout;
- explicit caller cancel;
- owner stopped;
- callee stopped;
- resource close;
- runtime shutdown;
- late reply after cancellation.

## Rock 1: Deadline Value

Add a tiny deadline helper.

Do this after Rock 2/3 unless the implementor needs it earlier for tests. Call
handles and cancellation are the load-bearing primitive; deadline is the helper
that makes chained cancellation/timeout easier to write.

Candidate:

```rust
#[derive(Clone, Copy, Debug)]
pub struct Deadline { ... }

impl Deadline {
    pub fn after(duration: Duration) -> Self;
    pub fn remaining(self) -> Duration;
    pub fn remaining_or_zero(self) -> Duration;
    pub fn expired(self) -> bool;
}
```

Rules:

- absolute deadline, not retry policy;
- no hidden cancellation;
- no hidden retry;
- easy conversion to existing `Duration` APIs;
- expired deadline is visible;
- live/sim clock truth must be named.

Clock rule:

- Pick one before coding:
  - live-only `Deadline`, documented as outside simulator/DST claims; or
  - runtime/sim clock-backed deadline, with simulator parity from day one.
- Do not ship a `Deadline::after` helper that examples can copy into
  replay-claimed code while secretly depending on `std::time::Instant`.
- If live-only is chosen, docs and examples must say "live deadline helper",
  not "Tina deterministic deadline".

Proof:

- `Deadline::after` counts down in live tests;
- expired deadline returns zero remaining;
- examples can pass one deadline through A -> B -> C without recomputing
  timeout math by hand.
- if live-only, simulator-facing examples do not use it as DST proof;
- if sim-backed, same deadline scenario replays under simulator time.

## Rock 2: Caller-Owned Call Handles

Add a way to start an isolate call and retain a caller-owned cancellation
handle.

Candidate:

```rust
let pending = call_with_handle(worker, msg, timeout)
    .reply(AppMsg::Done);

self.calls.insert(id, pending.handle())?;
pending.effect()
```

or:

```rust
let (effect, handle) = call_with_handle(worker, msg, timeout)
    .reply(AppMsg::Done);
```

Prefer the shape that keeps type inference and examples clean.

Rules:

- handle carries enough identity to reject stale/wrong-runtime/wrong-call use:
  call id, caller generation, callee address/generation, and shard identity if
  needed;
- handle is `#[must_use]`;
- handle does not allow replying;
- handle is not a pool lease and cannot release resources;
- handle is not a hidden queue slot by itself;
- dropping a handle does not cancel unless the API says so;
- handle is move-owned unless the design explicitly proves clone-safe handles;
- if handles are clone-safe, double cancel must still return typed truth rather
  than duplicate capacity reclamation;
- define whether handles may cross isolates/shards. If yes, prove stale
  generation and wrong-shard behavior;
- accepted call still follows normal Tina call/deferred-reply semantics.

Proof:

- call completes normally with handle unused;
- handle can be stored in isolate state;
- handle cannot be used after completion except to get `AlreadyCompleted`;
- handle type does not expose unsafe retyping or duplicate reply.
- stale handle is rejected;
- cross-shard handle behavior is tested or explicitly unsupported.
- compile-fail or doctest proof where practical: `CallHandle` cannot be used as
  a reply token, pool lease, or resource release token.

## Rock 3: Cancel One Pending Isolate Call

First public cancellation primitive: cancel a caller-owned isolate call.

Candidate:

```rust
cancel_call(handle).reply(AppMsg::Cancelled)
```

or a context/runtime method if an effect is too awkward:

```rust
ctx.cancel_call(handle).reply(AppMsg::Cancelled)
```

Semantics:

- If call is still queued/admitted but not delivered to the callee, settle
  caller side as cancelled and reclaim capacity.
- If call was delivered and the callee has not yet replied/captured, mark caller
  side cancelled; normal reply later is rejected visibly.
- If callee already captured a deferred reply, close the caller side; callee may
  still reply later, and that deferred reply is rejected visibly.
- If callee replies after cancellation, emit a trace fact like
  `CallReplyRejected { reason: CallerCancelled }`.
- If call already completed, return `AlreadyCompleted`.
- If handle is stale/closed, return a typed non-panic outcome.

State table the implementation must pin:

| Call state | Cancel result | Capacity result | Late work result |
|---|---|---|---|
| queued in caller/runtime, not delivered | `Cancelled` | caller/call capacity reclaimed now | none |
| delivered to callee, normal reply still possible | `Cancelled` | caller wait reclaimed now; callee may still run | normal reply rejected as `CallerCancelled` |
| deferred reply captured | `Cancelled` | caller wait reclaimed now; deferred slot closed from caller side | `reply_to` rejected as `CallerCancelled` |
| bridge/external work accepted | `Cancelled` | caller wait reclaimed now | worker may finish; late reply rejected; worker metrics record terminal result |
| already completed | `AlreadyCompleted` | already reclaimed | none |
| stale/wrong generation | typed rejection | no capacity change | none |

Pool-facing states for 067:

| Pool state | Cancel result | Resource result |
|---|---|---|
| waiting to acquire | remove waiter; free waiter cap | no resource touched |
| lease acquired, work not started | cancel wait only if there is a wait | user must explicitly release/retire lease |
| lease acquired, work in flight | stop waiting for work reply | late result or owner stop must explicitly release/retire |
| pool closing with waiters | waiters get `Closed` | no resource touched |
| pool closing with outstanding leases | cancel does not reclaim resource | late release is accepted-as-retired or rejected visibly by pool policy |

Do not build a helper that cancels a call and silently releases a lease. That is
too much magic, and it may reuse a poisoned resource.

Timeout integration:

- Existing Tina call timeout and explicit `cancel_call` should share the same
  slot-closing and capacity-reclamation machinery where possible.
- The cause must remain distinct: `CallerTimedOut` is not `CallerCancelled`.
- If timeout cannot be routed through exactly the same code path in first form,
  the phase must prove the two paths produce the same capacity truth and late
  reply rejection behavior.
- Do not fix explicit cancel while leaving timeout to leak or wait until an old
  deadline path.

Rules:

- timeout and explicit cancel are distinct;
- no fake cancellation of external work;
- no silent capacity leak;
- simulator parity for any shipped runtime behavior;
- late reply is visible truth, not success.

Proof:

- cancel before callee handles call;
- cancel after callee captures deferred reply;
- late reply after cancel is rejected visibly;
- cancellation reclaims caller/call capacity;
- double cancel returns typed outcome;
- caller timeout and explicit cancel have different trace facts;
- caller timeout and explicit cancel both reclaim capacity and reject late
  replies through the same semantics;
- simulator test for same public behavior.
- bridge-shaped proof or documented non-proof: cancel after a bridge accepts
  work, worker completes late, runtime rejects the late reply visibly, metrics
  count worker-terminal outcome.

## Rock 4: Bounded PendingCallSet

Every cancellable workflow will otherwise grow:

```rust
BTreeMap<RequestId, CallHandle>
```

Ship a small bounded helper for isolate state.

Candidate:

```rust
let mut calls = PendingCallSet::with_capacity(64);
calls.insert(id, handle)?;
let handle = calls.remove(&id);
let effects = calls.cancel_all_for::<I>(CancelCause::OwnerStopped);
```

Rules:

- bounded storage; use a fixed-capacity table/slab/ring or equivalent, not a
  normal `HashMap` that can grow forever;
- duplicate key is typed error;
- full table is typed error;
- remove on completion is explicit;
- cleanup on completion/cancel/timeout/owner stop is explicit;
- blessed cleanup pattern: every stored handle must have a completion,
  cancellation, or timeout continuation/message that removes its key. Do not
  rely on dropping the handle to clean the table;
- cancel-all produces visible cancel effects/outcomes;
- no helper owns the workflow.

Proof:

- full table rejects;
- completion removes;
- timeout continuation removes and frees capacity;
- explicit cancel removes and frees capacity;
- cancel-all frees capacity;
- fill table, cancel/drop/complete all entries, then admit new entries without
  stale `Full`;
- helper can be used without hiding per-call result handling.

## Rock 5: Owned Calls Cleanup

Stopping an isolate with pending calls should be boring and explicit.

Do not ship a mega shutdown helper. Compose small helpers:

```rust
let mut effects = Vec::new();
effects.extend(self.calls.cancel_all_for::<Self>(CancelCause::OwnerStopped));
effects.extend(self.pending_replies.drain_replies_for::<Self>(Reply::Closed));
effects.push(stop());
batch(effects)
```

Rules:

- owner stop cancels caller-owned pending calls;
- deferred reply drains remain separate;
- pool lease release remains separate;
- all capacity is reclaimed;
- trace names owner stop as the cause.

Proof:

- isolate stops with pending calls;
- callers settle now, not at original timeout;
- no stale cap after stop.

## Rock 6: Eiffel Cancellation Chain

Update existing Eiffel specimens first. Do not create a new specimen unless the
existing ones cannot honestly show the new model.

Primary target:

- `examples/eiffel_cancellation_chain`

Secondary target if deadline propagation lands cleanly:

- `examples/eiffel_backpressure_chain`

The specimen should show:

- one request fans out to downstream work;
- caller cancels mid-flight;
- accepted downstream work may finish late;
- late replies are rejected visibly;
- capacity is reclaimed;
- README explains why cancellation does not mean vanished work.

If the first-form API still makes domain `Stop` clearer for this specimen,
record that honestly and keep the domain `Stop` pattern.

## Order

1. Call handle shape.
2. Cancel one call, including timeout integration.
3. PendingCallSet.
4. Owner-stop cleanup.
5. Deadline value.
6. Eiffel cancellation chain update.

## Done Means

- explicit cancel and timeout are distinct in API and trace;
- cancelling a pending call reclaims capacity;
- late reply after cancel is visible and rejected;
- no external work is falsely described as cancelled;
- simulator parity exists for shipped behavior;
- examples/FINDINGS.md is updated;
- 067 can build bounded pools on top of this without inventing cancellation
  semantics.
