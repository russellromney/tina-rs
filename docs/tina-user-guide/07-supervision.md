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

## What To Test

For each supervised Tina port, test:

- child panic or stop
- parent observes or continues correctly
- restart happens when budget allows
- restart stops when budget exhausted
- queued messages do not secretly grow without bound

Tokio ports often have ad hoc retry loops. Tina should make policy visible.

If writing supervision feels too ceremonial, put it in ergonomics notes.
