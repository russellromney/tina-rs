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

- Existing result-carrying timer continuation stays.
- `then_event` is for the common "wake me later" path.
- The continuation returns a normal mailbox event.
- No state mutation happens inside the closure.
- The trace still records the timer completion as before.

Tests:

- A user enum can wake without a `SleepReply` field.
- Existing `sleep(...).then(|reply| ...)` behavior still works.
- Timer timeout/cancel/closed behavior, if any, is not hidden by
  `then_event`.

### Rock 2: Park Caller Authority

Current pain:

```rust
let request = call.into_request_context().into_deferred();
pending.try_insert(qid, request)?;
```

Ship a copied path on `PendingReplies`:

```rust
match self.pending.park(qid, call) {
    Ok(()) => start_work,
    Err(ParkError::Full(call)) => call.reply(Reply::Full),
    Err(ParkError::DuplicateKey(call, qid)) => call.reject(...),
}
```

Rules:

- `park` consumes `CallContext` only on success.
- On every failure, the original caller authority is returned.
- Failure must not strand the caller.
- No helper starts child work.
- Duplicate key stays typed.

Tests:

- Success parks and later replies the original caller.
- `Full` returns the caller authority; caller receives a typed reply/reject.
- duplicate key returns the caller authority.
- caller timeout/cancel is still visible and capacity is reclaimed.
- fill -> close/cancel -> refill works.

### Rock 3: Guarded Parked Replies

Current pain:

```rust
pending: PendingReplies<Id, Reply>,
leases: HashMap<Id, SharedLease>,
```

Ship a guarded parked slot:

```rust
self.pending.park_guarded(qid, call, guard)
```

and, if needed for lower-level deferred slots:

```rust
self.pending.insert_guarded(qid, deferred_reply, guard)
```

Use `guard`, not `lease`, in the public name. The guard is any RAII value that
must live while the caller is parked.

Implementation shape:

- Prefer extending `PendingReplies<K, R>` to `PendingReplies<K, R, G = ()>` if
  that stays source-compatible for existing users.
- If that makes bounds/docs ugly, add `GuardedPendingReplies<K, R, G>` as a
  sibling type and keep `PendingReplies<K, R>` as the common unguarded path.
- In either shape, storage stays a fixed-capacity slot table. No growing
  `HashMap`.

Rules:

- Guard drops exactly once.
- Guard drops on normal reply.
- Guard drops on drain/close.
- Guard drops when the caller is gone and the slot is swept.
- Failed admission returns both caller authority and guard.
- No sidecar map needed in migrated systems.

Tests:

- Drop counter proves normal reply releases the guard.
- Drop counter proves drain releases all guards.
- Drop counter proves caller-timeout sweep releases the guard.
- Failed `Full` / duplicate admission returns the guard.
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

Likely API:

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

If the implementation can make `reply_all` work cleanly for `R: Clone`, it may
add that alias. The docs must say when cloning is required. Do not silently
force `R: Clone` on the whole type.

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
- fill -> reply/drain -> refill.
- caller timeout/cancel cleanup reclaims capacity.

### Rock 5: `CancelableWork<K, Q, R>`

Current pain:

`PendingCancelableCallSet<K, Q, R>` is good when key identity is unique.
Real services often have natural keys with multiple live calls per key.

Ship:

```rust
let ticket = self.work.admit(key, pending)?;
let pending = self.work.finish(ticket)?;
for pending in self.work.drain() { ... }
```

Type name:

```rust
CancelableWork<K, Q, R>
```

Do not expose `Slab` in the copied name. Internally, use whatever bounded table
shape is right.

Rules:

- Natural key is grouping metadata.
- `WorkTicket<K>` is identity.
- Multiple live entries may share one key.
- Admission is bounded globally.
- Per-key cap ships in this phase.
- Child effect must still be gated by admission, like Phase 097.
- Failed admission returns the pending token so the caller can be answered.
- Cancel means Tina stops waiting / cancels Tina-owned work where possible.
- External work late completion remains visible as late/rejected truth.

Likely API:

```rust
CancelableWork::with_capacity(total)
CancelableWork::with_key_limit(total, per_key)
work.admit(key, pending) -> Result<WorkTicket<K>, AdmitWorkError<K, Q, R>>
work.finish(ticket) -> Option<PendingCancelableCall<K, Q, R>>
work.remove(ticket) -> Option<PendingCancelableCall<K, Q, R>>
work.drain() -> impl Iterator<Item = PendingCancelableCall<K, Q, R>>
work.snapshot()
```

Keep exact generic bounds boring. The easiest honest API is to return the
removed `PendingCancelableCall` and let user code call `.cancel(...)` so the
continuation message is explicit. Add direct `cancel_with(...)` only if it makes
the copied path clearer without trait soup.

`finish` and `remove` are storage verbs. They do not reply to the original
caller by themselves. The service still answers through the returned pending
token's request context, so reply policy stays visible.

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
- global full returns pending token.
- per-key full returns pending token.
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

Use existing `CapacitySurfaceReport` where possible. Names must be user-settable
with `.named("service.waiters")`.

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

Optional if the changes are natural:

- `examples/systems/system_metrics_shipper`
- `examples/systems/mini_saas_api`

Do not rewrite systems just for churn. Each migration must delete repeated glue
or prevent a known mistake.

## Documentation

Update copied-path docs:

- request/reply multi-turn section
- service patterns
- capacity/overload docs if guarded parking changes examples
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
