# Supervision

Supervision is how Tina says:

```text
child died
policy decides restart or stop
budget prevents restart storm
```

Use this for connection workers, partition workers, protocol handlers, and
other owned children.

## Child Without Restart

Use `ChildDefinition` when the child is just work.

```rust
spawn(ChildDefinition::new(Connection { stream }, 16))
```

If it stops, it stops.

When the parent needs to talk to the child after spawning it, use
`spawn_observed`.

```rust
enum ParentMsg {
    StartChild,
    ChildStarted(Result<ChildRef<ChildMsg, ChildReply>, SpawnObservedError>),
}

spawn_observed(ChildDefinition::new(Child::default(), 16))
    .then(ParentMsg::ChildStarted)
```

The result is delivered as an ordinary later parent message. `ChildRef` carries
the typed `Address<ChildMsg, ChildReply>` plus its generation. A restart creates
a fresh child incarnation; the old address/ref is stale and sends through it
close or reject like any stale address.

`SpawnObservedError` is for spawn construction rejection, such as a zero
mailbox capacity. If the parent mailbox itself is full or closed when the
continuation should be delivered, Tina records the normal send rejection in
the trace. It does not add a hidden queue or bypass the parent's mailbox to
force the continuation through.

## Restartable Child

Use `RestartableChildDefinition` when the child is part of service health.

Shape:

```rust
use tina::prelude::*;
use tina_supervisor::{RestartBudget, RestartPolicy, SupervisorConfig};

#[tina::isolate(
    message = ParentMsg,
    spawn = RestartableChildDefinition<Worker>,
    shard = AppShard
)]
impl Parent {
    fn handle(&mut self, msg: ParentMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ParentMsg::StartChild => spawn(RestartableChildDefinition::new(
                || Worker::default(),
                32,
            )),
        }
    }
}
```

Then runtime/sim supervision config decides restart behavior.

Exact wiring depends on the runner being used. Look at existing supervision
tests before making a new shape.

## Budget

Restart needs budget.

Grug rule:

```text
restart is medicine
too much medicine is poison
```

Use small budgets in tests to prove failure is contained.

Two budget shapes ship:

- `RestartBudget::new(max)` — runtime-lifetime cap. After `max`
  restarts, the next failure is terminal.
- `RestartBudget::within(max, window)` — windowed cap. Restart count
  resets after the window passes.

Use the lifetime cap for simple "never restart more than N times" services.
Use the windowed cap for long-lived workers where a burst is bad but one
failure per hour is acceptable.

## What To Test

For each supervised Tina port, test:

- child panic or stop
- parent observes or continues correctly
- restart happens when budget allows
- restart stops when budget exhausted
- queued messages do not secretly grow without bound

Tokio ports often have ad hoc retry loops. Tina should make policy visible.

If writing supervision feels too ceremonial, put it in ergonomics notes.
