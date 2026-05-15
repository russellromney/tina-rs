# Job Queue

A bounded job queue with `N` worker isolates spawned as the queue's children.
Callers `Submit` and block until the job finishes. The queue parks each
caller in `PendingReplies` keyed by job id, dispatches work to idle workers
via `call_cancelable`, and stashes every in-flight call handle in a
`PendingCallSet`. A `Poison` payload panics the worker; the queue sees
`CallOutcome::Closed` on the call, respawns the worker into the same slot,
and retries the job until its `max_retries` budget is gone.

## Run

```bash
cargo run --manifest-path examples/systems/system_job_queue/Cargo.toml
cargo test --manifest-path examples/systems/system_job_queue/Cargo.toml
```

## Findings

What felt good:
- `register_with_capacity_using` plus `spawn_observed(ChildDefinition::new(...))`
  is the right shape for a parent that needs both its own address and typed
  child references at startup.
- Routing dispatch through `call_cancelable` instead of `try_send` makes a
  worker death visible: `CallOutcome::Closed` arrives without any extra
  watchdog. The retry-or-fail decision then lives in one place.
- `PendingReplies` for parked `Submit` callers and `PendingCallSet` for
  in-flight worker handles are two clean primitives at the same time, on
  different sides of the same job lifecycle.

What felt rough:
- There is no in-isolate hook for "my child restarted." The parent isolate
  has to infer worker death indirectly (here, from `CallOutcome::Closed`)
  and respawn manually. `runtime.observe_child_restarted(parent)` exists,
  but only outside the isolate. A first-class child-lifecycle event
  (`ChildStopped`, `ChildRestarted`) inside `handle` would remove a class
  of bugs where a queue forgets a dead worker until it next dispatches.
- Bootstrapping the worker pool requires a one-shot `QueueMsg::Bootstrap`
  message because spawn effects only return from `handle`, not from the
  isolate constructor. The host has to send the message after registration.
  A `register_then_send` helper would remove the boilerplate.
- Cancel-while-running needs *two* coordinated actions (send `WorkerMsg::Cancel`
  to nudge the worker, leave the in-flight call handle in `PendingCallSet`
  so the eventual `Cancelled` reply still routes through one place). Easy
  to forget one half. A `cancel_via_worker(worker_addr, id, handle)`
  helper that does both atomically would make this less footgun-shaped.
- `call_cancelable(...).then(...)` returns `(Effect, CallHandle)`. Pairing
  the handle with isolate state always reads as boilerplate; a single
  `call_cancelable_into_set(set, key, ...)` builder would express the
  intent directly.
- Reused job ids would silently collide with `PendingCallSet::insert`'s
  `DuplicateKey` rule. Monotonic ids work here but the failure mode is
  worth a louder name than "queue accounting bug."

Tina capability pulled:
- Parent isolate that supervises its children's address book.
- `spawn_observed` with `ChildDefinition`.
- `call_cancelable` from one isolate to another, with handles stashed in
  `PendingCallSet`.
- `PendingReplies` for caller-authority parking across multi-turn work.
- `CallContext` request/reply surface on both queue and worker.
- Runtime-owned `sleep` as the worker's only async surface.

Suggested follow-up:
- In-isolate child-stopped / child-restarted events (see above).
- `register_then_send(msg)` shorthand for "spawn me, then deliver this
  bootstrap message."
- A `WorkerLane` helper that combines the worker-address slot, the busy
  marker, and the pending call handle so the queue does not have to keep
  three parallel `Vec`s in sync.

Verdict:
- keep
