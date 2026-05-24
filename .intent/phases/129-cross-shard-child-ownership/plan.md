# Phase 129: Cross-Shard Child Ownership

## Status

- Future implementation plan.
- Runs after Phase 120 or beside it if the PR does not edit the copied service
  skeleton.
- One PR when executed.

## Purpose

Make a child spawned onto another local shard behave like an owned child.

User story:

```text
My parent can spawn a worker/session on another shard, learn its typed address,
stop it, observe restart/address changes, and shut it down without trace
spelunking or a side registry.
```

## Starting Facts

- `spawn_observed(child).on_shard(shard).then(...)` already exists.
- The remote spawn path currently returns a typed `ChildRef` and emits
  `ChildStarted`.
- Cross-shard spawned children are explicitly not owned today. `StopChildren`
  and supervision do not reach them.
- Same-shard `on_shard(self_shard)` already degenerates into an ordinary owned
  local child. Keep that behavior.
- Remote transport is bounded. All new child-control traffic must use bounded
  shard-pair queues and typed `Full` / `Closed` truth.

## Does Not Include

- no distributed remoting
- no cluster membership
- no shard failover or shard restart
- no hard OS thread pinning
- no global child registry
- no hidden unbounded owner-to-child queue
- no cross-shard restartable child factory that crosses threads unless the
  factory is made `Send` and tested

## Decisions

- Scope is local in-process multi-shard only.
- Ownership is recorded on both sides:
  - owner shard: parent owns a child record with child shard/id/generation
  - child shard: child knows its remote owner address
- A cross-shard child has a stable per-parent ordinal, just like local
  children.
- `StopChildren` reaches remote children by sending bounded remote child-control
  envelopes.
- Owner stop runs child cleanup too. It must not orphan a child that has already
  been admitted on the target shard.
- If owner stops while a spawn request is still in flight, the destination must
  either:
  - not create the child, or
  - create it and immediately stop it through recorded owner-stop truth.
  It must not leave the current orphan behavior.
- Restart/address-change truth is owner-visible:
  - old address becomes stale
  - replacement address is available through `ChildRestarted` waiters and
    `ChildLifecycleReport`
  - callers do not need to search raw trace to find the new address
- First form does not make `RestartableChildDefinition` cross shards unless the
  recipe can be stored and re-run on the child shard with normal `Send` bounds.
  If that is not true in the current code, remote restart must be a typed
  `RestartChildSkipped { reason: RemoteNotRestartable }` fact by adding
  `RestartSkippedReason::RemoteNotRestartable`. Do not fake restart by
  respawning on the owner shard.

## Implementation Shape

Keep the existing public spawn spelling:

```rust
spawn_observed(ChildDefinition::new(worker, 8))
    .on_shard(worker_shard)
    .then(ParentMsg::WorkerStarted)
```

Add runtime-owned remote ownership records:

- extend the child record to carry child shard, remote-owner flag, and owner
  address/generation
- add bounded remote envelopes for child control:
  - stop child
  - child stopped ack/report
  - restart child request or restart skipped report
  - child address changed report
- keep the destination-shard child stop path using normal stop semantics, so
  pending calls/captures/resources settle exactly like a local stop
- make owner-side reports consume acks/reports through ordinary bounded owner
  delivery, not a hidden side channel

Add user-facing report/query helpers:

- `ChildRef` remains the typed address for one incarnation.
- Add `ChildLifecycleReport`, built from runtime child records for live
  runtimes and from trace/shutdown facts for terminal reports. It answers:
  - current known child address, shard, generation, ordinal
  - state: starting / live / stopping / stopped / restart skipped / restarted
  - last stop/restart reason
  - pending remote control count
- Extend `ChildRestarted` with `new_shard` so existing host waiters can update
  a full typed address for local and remote children.
- Integrate remote children into `SupervisorReport` instead of making users
  read a second report for common supervision counts.
- Add these query paths:
  - `Runtime::child_lifecycle_report(parent)`
  - `MultiShardRuntime::child_lifecycle_report(parent)`
  - `ThreadedRuntime::child_lifecycle_report(parent) -> Result<..., ThreadedRuntimeError>`
  - `ThreadedMultiShardRuntime::child_lifecycle_report(parent) -> Result<..., ThreadedRuntimeError>`
  They must return typed unavailable/stopped errors instead of partial guesses.

Trace vocabulary:

- keep existing `Spawned` on child shard and `ChildStarted` on owner shard
- add append-only stable trace variants for:
  - remote child stop requested
  - remote child stopped
  - remote child restart requested
  - remote child restart skipped with `RemoteNotRestartable`
  - remote child restarted/address changed
- include child shard on any new child lifecycle event that can name a remote
  child
- do not renumber existing stable hash tags

## Required Proof

Live multi-shard tests:

- parent on shard A spawns child on shard B and learns typed `ChildRef`
- `StopChildren` on parent stops the remote child; sending to old child returns
  `Closed`
- parent `batch([spawn_on(B), stop_children(), stop()])` does not orphan the
  child
- parent `batch([spawn_on(B), stop()])` either cancels spawn before child
  creation or immediately stops the created child; the current orphan proof must
  be rewritten to expect no orphan
- owner stops after spawn request is routed but before reply lands; no orphan,
  no continuation into a dead owner, and final report names what happened
- owner mailbox full when child report comes back records `Full`, not silent
  loss
- stale child address after restart/stop returns `Closed`
- `SupervisorReport` includes the remote child counts and state
- `ChildRestartedWaiter` includes `new_shard` for remote children
- live `ChildLifecycleReport` and terminal report agree on child state after
  shutdown

Simulator tests:

- the same spawn/stop/restart-address sequence is deterministic under
  explicit stepping
- replay hash changes only for the new remote child-control facts, not from
  reordered unrelated events
- bounded shard-pair full during remote stop/restart produces typed pressure
  and no hidden retry

Compile/API tests:

- ordinary same-shard child code still compiles unchanged
- `spawn_observed(...).on_shard(...)` still requires `Send` payloads
- non-`Send` restart recipe cannot cross shards silently
- same-shard `.on_shard(self_shard)` still creates an owned local child and
  emits no cross-shard `ChildStarted`
- old code using `ChildRestarted { child_ordinal, new_isolate, new_generation,
  .. }` still compiles because the type is non-exhaustive

Specimen proof:

- add or refresh one small specimen where a supervisor owns workers on two
  shards, restarts or stops them, and prints a child lifecycle report
- README must show the copied path and the expected report line

Verification commands:

```bash
cargo test -p tina-runtime multishard_cross_shard_spawn -- --nocapture
cargo test -p tina-runtime cross_shard_child_ownership -- --nocapture
cargo test -p tina-sim cross_shard_child_ownership -- --nocapture
cargo test -p tina-runtime --test application_surface child -- --nocapture
cargo fmt --all --check
cargo clippy -p tina-runtime --tests -- -D warnings
```

## Traps

- Do not add a user-maintained `Arc<Mutex<HashMap<ChildId, Address>>>`.
- Do not make remote child cleanup best-effort without a visible report.
- Do not turn remote `Full` into `Closed` or a generic error.
- Do not execute a child restart on the owner shard by accident.
- Do not hide replacement addresses only in trace events; give users a typed
  report/continuation path.
- Do not weaken same-shard local supervision while adding remote support.
