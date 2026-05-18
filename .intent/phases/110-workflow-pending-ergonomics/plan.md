# Phase 110: Workflow Pending Ergonomics

## Status

- IDD implementation phase.
- Runs after phases 095, 097, 101, 107, and 108.
- Can run in parallel with protocol/lifecycle work if ownership stays narrow:
  `tina`, `tina-runtime`, docs, and selected systems.
- Do not start a broad service-skeleton rewrite here. This is helper work
  backed by systems that already exist.

## Grug Truth

A Tina workflow is still:

```text
request comes in
service decides admission
service parks caller authority if needed
runtime work happens
continuation returns as a normal event
service mutates state and replies
```

This phase removes repeated glue. It does not hide the workflow.

Helpers may shorten:

- parking a caller
- waking later
- holding a guard while parked
- replying or closing waiters
- tracking cancelable work by natural key

Helpers must not hide:

- caller authority
- bounded storage
- `Full` / `Closed` / `Timeout`
- cancellation truth
- late replies
- trace and capacity facts

## Goal

Make the copied multi-turn service path boring and hard to wire wrong.

The names are pinned here. Use user-facing verbs:

- `then_event`: wake me later with an event
- `park`: this caller waits here
- `park_guarded`: this caller waits and owns a guard
- `WaitList`: many callers wait for a key
- `CancelableWork`: live cancelable work grouped by key

Do not expose implementation names like `Slab` on the copied path.

## Non-Goals

- No `flow!`.
- No fake async/await surface.
- No hidden callbacks that mutate service state.
- No hidden retry.
- No hidden queue.
- No automatic cancellation policy.
- No service skeleton rewrite.
- No runtime shared-scope registry.
- No public request/internal event split.
- No bridge classifier unification.

## Rocks

### Rock 1: Unit Timer Event

Current pain:

```rust
enum Msg {
    Wake { id: u64, result: SleepReply },
}
```

when the handler never reads `result`.

Ship:

```rust
sleep(delay).then_event(move || Msg::Wake { id })
```

Rules:

- `then_event` is a sleep/timer helper, not a blanket `TypedCall<()>`
  helper. Do not let it hide errors from file, process, signal, TCP, TLS, or
  bridge calls that also return `()`.
- Introduce a small sleep-specific wrapper returned by `sleep(delay)`. It keeps
  existing `.then(...)` behavior and adds `.then_event(...)`. Do not put
  `.then_event(...)` on all `TypedCall<()>`.
- Existing `sleep_then(after, message)` stays and delegates to the same path.
  Docs prefer the fluent form when the message needs captured values.
- Existing result-carrying timer continuation stays.
- `then_event` is for the common "wake me later" path.
- The continuation returns a normal mailbox event.
- No state mutation happens inside the closure.
- The trace still records the timer completion as before.

Tests:

- A user enum can wake without a `SleepReply` field.
- Existing `sleep(...).then(|reply| ...)` behavior still works.
- A non-timer `TypedCall<()>` cannot use `then_event`.
- `sleep_then` and `sleep(...).then_event(...)` produce the same timer
  behavior.
- Timer cancellation/closed behavior, if any, is not hidden by `then_event`.

### Rock 2: Park Caller Authority

Current pain:

```rust
let request = call.into_request_context().into_deferred();
pending.try_insert(qid, request)?;
```

Ship copied paths on `PendingReplies`:

```rust
match self.pending.park_request(qid, call) {
    Ok(ticket) => start_work(ticket),
    Err(ParkError::Full(call)) => call.reply(Reply::Full),
    Err(ParkError::DuplicateKey(call, qid)) => call.reject(...),
}
```

and, for the lower-level `CallContext` path:

```rust
self.pending.park_call(qid, call_context)
```

Rules:

- `park_request` consumes `RequestCall` only on success.
- `park_call` consumes `CallContext` only on success.
- On every failure, the original caller authority is returned.
- `park_request` checks duplicate/capacity before capturing the caller, so
  `Full` and `DuplicateKey` can return the original `RequestCall`.
- Failure must not strand the caller.
- No helper starts child work.
- Duplicate key stays typed.
- Success returns a `ParkTicket<K>`.
- The copied reply/take path uses the ticket, not only the key, so stale
  continuations cannot remove a newer parked caller after key reuse.
- Tickets have private fields and carry a generation/slot identity. User code
  can carry a ticket but cannot forge one.
- Existing key-only `try_insert` / `take(&key)` may remain as lower-level
  escape hatches, but migrated copied paths should carry tickets.

Required API:

```rust
CallContext::try_into_request_context(self) -> Result<RequestContext<I::Reply>, (Self, TakeReplySlotError)>
RequestCall::try_capture(build) -> Result<RequestEffect<I>, (Self, TakeReplySlotError)>
pending.park_request(key, RequestCall<'_, I>) -> Result<ParkTicket<K>, ParkError<K, I>>
pending.park_call(key, CallContext<'_, I>) -> Result<ParkTicket<K>, ParkCallError<K, I>>
pending.take_ticket(ticket) -> Result<DeferredReply<R>, TakeParkedError<K>>
pending.reply_ticket(ticket, reply) -> Result<Effect<I>, ReplyParkedError<K, R>>
```

Required error variants:

```rust
ParkError::Full { key, call }
ParkError::DuplicateKey { key, call }
ParkCallError::NoCaller { key, call }
ParkCallError::CrossShardUnsupported { key, call }
ParkCallError::Full { key, call }
ParkCallError::DuplicateKey { key, call }
TakeParkedError::Missing
TakeParkedError::StaleTicket
```

`reply_ticket` is public convenience over `take_ticket` plus existing
`reply_to`. `take_ticket` is also public for services that need custom reply
policy. Do not drop ticket identity from the public path.

Tests:

- Success parks and later replies the original caller.
- `Full` returns the caller authority; caller receives a typed reply/reject.
- duplicate key returns the caller authority.
- `RequestCall::try_capture` returns the original `RequestCall` on failure.
- `CallContext::try_into_request_context` returns the original `CallContext`
  on failure.
- stale key-only completion cannot remove a newer parked caller when the copied
  ticket path is used.
- `ParkTicket` fields are private; doctest/compile-fail proves user code
  cannot forge one.
- caller timeout/cancel is still visible and capacity is reclaimed.
- fill -> close/cancel -> refill works.

### Rock 3: Guarded Parked Replies

Current pain:

```rust
pending: PendingReplies<Id, Reply>,
leases: HashMap<Id, SharedLease>,
```

Ship a guarded parked slot in a sibling type:

```rust
let ticket = self.pending.park_request_guarded(qid, call, guard)?;
```

and the lower-level call/deferred paths:

```rust
self.pending.park_call_guarded(qid, call_context, guard)
self.pending.insert_deferred_guarded(qid, deferred_reply, guard)
```

Use `guard`, not `lease`, in the public name. The guard is any RAII value that
must live while the caller is parked.

Implementation shape:

- Add `GuardedPendingReplies<K, R, G>`.
- Keep `PendingReplies<K, R>` as the common unguarded path.
- Storage is a fixed-capacity slot table. No growing `HashMap`.
- `G` is any RAII guard value. The helper never calls methods on it.

Rules:

- Guard drops exactly once.
- Guard drops on normal reply.
- Guard drops on drain/close.
- Guard drops when the caller is gone and the slot is swept.
- Failed admission returns both caller authority and guard.
- Success returns a ticket; copied take/reply paths use the ticket.
- No sidecar map needed in migrated systems.

Tests:

- Drop counter proves normal reply releases the guard.
- Drop counter proves drain releases all guards.
- Drop counter proves caller-timeout sweep releases the guard.
- Failed `Full` / duplicate admission returns the guard.
- stale key-only completion cannot steal a newer guarded slot.
- guarded ticket fields are private; doctest/compile-fail proves user code
  cannot forge one.
- No double drop.

### Rock 4: `WaitList<K, R>`

Current pain:

Services need "many callers wait for this natural key" and rebuild it with
maps plus `PendingReplies`.

Ship:

```rust
let ticket = self.waiters.park(key, call)?;
self.waiters.reply_all_clone(&key, Reply::Hit(value));
self.waiters.close_all_clone(&key, Reply::Closed);
```

Type name:

```rust
WaitList<K, R>
```

Home:

```rust
tina_runtime::wait_list::WaitList
```

Rules:

- One global capacity.
- Per-key capacity is optional at construction time. Both constructors ship:
  no per-key limit, and explicit per-key limit.
- FIFO per key.
- `WaitError::Full(call)` for global full.
- `WaitError::KeyFull(call, key)` for per-key full.
- Natural key is user-facing grouping.
- Internal ticket prevents stale completions from touching the wrong waiter.
- No unbounded waiter growth.

Required API:

```rust
WaitList::with_capacity(total)
WaitList::with_key_limit(total, per_key)
waiters.park(key, call) -> Result<WaitTicket<K>, WaitError<K, R>>
waiters.reply_one(ticket, reply)
waiters.reply_all_clone(&key, reply)       // requires R: Clone
waiters.reply_all_with(&key, || reply)     // factory for non-Clone replies
waiters.close_all_clone(&key, reply)       // requires R: Clone
waiters.close_all_with(&key, || reply)     // factory for non-Clone replies
waiters.drain_all_with(|| reply)
waiters.snapshot()
```

Do not add a bare `reply_all` alias. The copied API says `reply_all_clone` or
`reply_all_with`, so clone requirements stay visible.

Implementation shape:

- Fixed-capacity slot table with `(ticket, key, DeferredReply<R>)`.
- Linear scans are fine. This helper is for tens/low hundreds of waiters.
- Per-key counts can be computed by scan.
- Empty keys are omitted from snapshots.
- No ordinary `HashMap<K, Vec<_>>` whose buckets grow independently of the
  global cap.

Tests:

- FIFO order per key.
- global cap full.
- per-key cap full.
- reply one by ticket.
- reply all by key.
- close all by key.
- drain all.
- stale ticket cannot remove/reply a newer waiter.
- `WaitTicket` fields are private; doctest/compile-fail proves user code
  cannot forge one.
- fill -> reply/drain -> refill.
- caller timeout/cancel cleanup reclaims capacity.

### Rock 5: `CancelableWork<K, Q, R>`

Current pain:

`PendingCancelableCallSet<K, Q, R>` is good when key identity is unique.
Real services often have natural keys with multiple live calls per key.

Ship:

```rust
let ticket = self.work.admit(pending)?;
let pending = self.work.take(ticket)?;
for pending in self.work.drain() { ... }
```

Type name:

```rust
CancelableWork<K, Q, R>
```

Home:

```rust
tina_runtime::call::CancelableWork
```

Do not expose `Slab` in the copied name. Internally, use whatever bounded table
shape is right.

Rules:

- Natural key is grouping metadata.
- `WorkTicket<K>` is identity.
- Tickets have private fields and carry generation/slot identity. User code can
  carry a ticket but cannot forge one.
- Multiple live entries may share one key.
- Admission is bounded globally.
- Per-key capacity is optional at construction time. Both constructors ship:
  no per-key limit, and explicit per-key limit.
- Child effect must still be gated by admission, like Phase 097.
- Failed admission returns the pending token so the caller can be answered.
- Cancel path removes the token, calls `PendingCancelableCall::cancel(...)`,
  and answers the original caller from the cancel continuation.
- External work late completion remains visible as late/rejected truth.

Required API:

```rust
CancelableWork::with_capacity(total)
CancelableWork::with_key_limit(total, per_key)
work.admit(pending) -> Result<WorkTicket<K>, AdmitWorkError<K, Q, R>>
work.take(ticket) -> Option<PendingCancelableCall<K, Q, R>>
work.drain() -> impl Iterator<Item = PendingCancelableCall<K, Q, R>>
work.snapshot()
```

`PendingCancelableCall` already carries its key. `CancelableWork::admit(...)`
uses that key as the natural grouping key; do not require users to pass the same
key twice.

Keep exact generic bounds boring. The honest API returns the removed
`PendingCancelableCall`. User code then calls `.cancel(...)` or
`.into_request_context()`, so the continuation/reply message is explicit. Do
not add `cancel_with(...)` in this phase.

`take` and `drain` are storage verbs. They do not reply to the original caller
by themselves. The service still answers through the returned pending token's
request context, so reply policy stays visible.

Implementation shape:

- Fixed-capacity slot table with `(WorkTicket<K>, key, PendingCancelableCall)`.
- `WorkTicket<K>` carries enough identity to remove by ticket without a fresh
  key lookup, or carries `(key, generation)` if that is simpler.
- Multiple entries may have the same natural key.
- Per-key counts can be computed by scan.
- No growing map of vectors behind the global cap.

Tests:

- two live entries for the same natural key.
- cancel one entry without touching its sibling.
- stale completion cannot remove a newer ticket.
- `WorkTicket` fields are private; doctest/compile-fail proves user code
  cannot forge one.
- global full returns pending token.
- per-key full returns pending token when using `with_key_limit`.
- drain replies/cancels every parked caller.
- fill -> cancel/drain -> refill.

### Rock 6: Pressure Snapshots For New Helpers

Every new bounded helper needs the same boring visibility:

```rust
helper.capacity()
helper.len()
helper.high_water()
helper.full_rejects()
helper.capacity_report()
```

Use `CapacitySurfaceReport` for every helper snapshot. Count-based helpers use
`CapacitySurfaceReport::count`. Any future weighted helper must use
`CapacitySurfaceReport::weighted`. Names must be user-settable with
`.named("service.waiters")`.

Tests:

- high water rises under load.
- full reject count increments on full.
- capacity report excludes reclaimed caller-gone slots.
- migrated system exposes at least one new helper surface in its existing
  pressure/capacity line.

## System Migrations

Migrate these systems where the new helpers remove real code:

- `examples/systems/ergonomics_playground`
- `examples/systems/system_cache_with_fill`
- `examples/systems/system_api_gateway_limits`
- `examples/systems/system_soak_http_db`

Do not migrate `system_metrics_shipper` or `mini_saas_api` in this phase unless
one of the required helper tests needs them. Each migration must delete repeated
glue or prevent a known mistake.

## Documentation

Update copied-path docs:

- `docs/tina-user-guide/04-request-reply.md`
- `docs/tina-user-guide/10-service-patterns.md`
- `docs/tina-user-guide/06-boundedness-and-overload.md`
- systems README notes
- `examples/FINDINGS.md`: close or update findings for `SleepReply`, caller
  parking, guarded pending replies, and natural-key waiters
- `CHANGELOG.md`: short entry under Unreleased

Docs must show the default names:

- `then_event`
- `park`
- `park_guarded`
- `WaitList`
- `CancelableWork`

## Required Proof

Unit tests are not enough. This phase needs helper tests plus user-shaped
systems.

Required:

- helper unit tests for every success/failure path above.
- at least one compile-check or doctest proving `then_event` does not require a
  `SleepReply` field in the user's enum.
- runtime test proving `park` full/duplicate returns caller authority and the
  caller does not time out.
- runtime test proving copied `park` ticket removal prevents stale-key ABA.
- runtime test proving guarded park releases guards on normal reply, drain, and
  caller-gone sweep.
- runtime test proving `WaitList` FIFO, global cap, per-key cap, stale ticket,
  and refill.
- runtime test proving `CancelableWork` same-key siblings and stale completion
  safety.
- capacity-report tests for every new bounded helper.
- migrated system smoke tests still pass.
- at least one migrated system asserts the same pressure/reply facts as before,
  so the helper did not hide capacity truth.
- one migrated system includes a regression where a parked caller would have
  timed out before this phase, and now receives the intended reply.

## Hostile Review Checklist

For every helper, answer in code/tests:

```text
Who owns caller authority?
Where is bounded storage?
What happens on Full?
What happens on duplicate key?
What happens on caller timeout/cancel?
What happens on owner shutdown?
What trace/capacity fact proves it?
Can stale completion remove new work?
Can the helper start work before admission?
```

If any answer is fuzzy, do not ship that helper.

## Done Means

The common multi-turn service path is shorter, but still visibly Tina:

- caller authority is parked deliberately
- capacity is named and bounded
- delayed work returns as events
- waiters and cancelable work have tickets
- shutdown and timeout reclaim capacity
- systems use less sidecar glue without losing pressure or trace truth
