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

`SpawnObservedError` is for spawn construction rejection (zero mailbox
capacity, factory panic, destination unavailable) and for terminal-observation
admission failure (`ParentMailboxFull` / `ParentMailboxClosed`) when a spawn
with `.then_result` / `.then_service_result` cannot reserve a parent mailbox
slot for the eventual child terminal.

Lifecycle continuations (initial result, restart refresh, and those admission
errors) are ordinary parent messages. When the parent's bounded mailbox is full
— including when a terminal-delivery reservation holds the last free slot —
Tina parks that one lifecycle fact in a **priority overflow lane** and drains
it on a later step ahead of ordinary ingress. That is not a hidden admission
queue and not a retry loop: capacity stays bounded, and a closed parent still
rejects. Cross-shard observed delivery does not use the overflow lane.

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

When the parent needs to keep talking to replacement children, observe the
restartable spawn and map both lifecycle points into parent messages:

```rust
spawn_observed(RestartableChildDefinition::new(|| Worker::default(), 32))
    .then_with_restarts(
        ParentMsg::ChildStarted,
        ParentMsg::ChildRestarted,
    )
```

`ChildStarted` receives the initial
`Result<ChildRef<WorkerMsg>, SpawnObservedError>`. `ChildRestarted` receives a
fresh `ChildRef<WorkerMsg>` after each successful replacement. Store that ref
in the parent and route work through the parent; do not reconstruct an address
from untyped isolate/generation fields. The old ref remains honestly stale.

If the restartable child's initial isolate or bootstrap factory panics, no
child is published and `ChildStarted` receives
`Err(SpawnObservedError::FactoryPanicked)`. The runtime and simulator remain
available for later work.

Both continuations use the parent's ordinary bounded mailbox, with the same
priority overflow rule as initial observed spawn when full under reservation
pressure (see above). A closed or stopped parent still rejects.

If a replacement factory panics, Tina records `FactoryPanicked` and does not
invoke the restart continuation. A later restart attempt may use the retained
recipe again.

When the spawn also observes the child's terminal result:

```rust
spawn_observed(RestartableChildDefinition::new(|| Worker::default(), 32))
    .then_service_result(ParentEvent::ChildStopped)
    .then_service_event_with_restarts(
        ParentEvent::ChildStarted,
        ParentEvent::ChildRestarted,
    )
```

Admission reserves one parent mailbox slot for that generation's terminal
delivery. If reservation is `Full` or `Closed`, spawn or restart is not
admitted and the parent gets that typed outcome (via the lifecycle delivery
path above). On `stop_with`, the runtime maps the payload once into the parent
event; plain stop, type mismatch, stale generation, duplicate settlement,
parent stop, and shutdown dispose the result with a typed trace reason.

The effect has the same semantics in the live runtime and simulator.

For a split event/request service, keep the routing envelope out of the
application and map both lifecycle points directly into events:

```rust
spawn_observed(RestartableChildDefinition::new(|| Worker::default(), 32))
    .then_service_event_with_restarts(
        ParentEvent::ChildStarted,
        ParentEvent::ChildRestarted,
    )
```

The result and bounded-delivery semantics are identical to
`then_with_restarts`; only the framework-owned `ServiceMessage::Event` wrapping
is hidden.

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
