# specimen_dynamic_worker_pool

Tokio-vs-Tina dynamic worker pool with join-all aggregation. A
coordinator spawns 4 workers, gives each a 4-element slice of a
fixed input, joins their partial sums into one total. Both sides
produce `total_sum = 136 = sum(1..=16)`.

## Run

```sh
cargo run --manifest-path examples/specimen_dynamic_worker_pool/Cargo.toml -- both
cargo test --manifest-path examples/specimen_dynamic_worker_pool/Cargo.toml
```

```
side=tokio results_collected=4 total_sum=136 exit_clean=true
side=tina  results_collected=4 total_sum=136 exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape

Standard `tokio::task::JoinSet`:

```rust
let mut joins: JoinSet<u64> = JoinSet::new();
for slice in chunks {
    joins.spawn(async move { slice.iter().sum() });
}
while let Some(joined) = joins.join_next().await {
    total += joined?;
}
```

`JoinSet` is the canonical answer for "spawn N short-lived tasks and
collect their results in arbitrary order." Failure is one
`JoinError` per task; partial-failure aggregation is done in the
loop body.

## Tina shape

The coordinator is one isolate. Children are spawned via
`spawn(ChildDefinition::new(...).with_initial_message(WorkerMsg::Compute))`.
Each child's `Send` type is `Outbound<CoordMsg>` so it can post its
partial back to the coordinator.

The children need the coordinator's address so they can send partials
back. `LocalSystem` supplies that address to an address-aware root
constructor before registration completes:

```rust
let coord_addr = app.register_root_using(capacity, |self_addr| {
    Coordinator { self_addr, chunks, /* ... */ }
})?;
app.try_send(coord_addr, CoordMsg::Start)?;
```

The coordinator's `Start` arm constructs each child with
`parent: self.self_addr` and one bounded slice of work, then builds a
bounded batch of `spawn(...)` effects:

```rust
CoordMsg::Start => {
    let chunks = BoundedItems::try_from_iter(self.expected as usize, self.chunks.drain(..))
        .expect("worker chunks are capped by WORKER_COUNT");
    bounded_batch(chunks.map_effects(|chunk| {
        spawn(ChildDefinition::new(
            Worker { parent: self.self_addr, chunk },
            4,
        ).with_initial_message(WorkerMsg::Compute))
    }))
}
```

Each `WorkerDone(partial)` arrives in the coord's mailbox like any
other message; once the coord has heard from every child it
`stop_with(self.report)` and the host's `observe_result::<Report>`
waiter resolves.

## Discussion

What feels better:

- **Workers as isolates.** Each worker has its own bounded mailbox,
  its own owned state, its own trace identity. There is no
  `tokio::spawn(async {...})` closure that captures whatever the
  surrounding scope happens to have; there is a typed `Worker`
  struct with documented fields.
- **Aggregate is just messages.** `WorkerDone(partial)` flows through
  the same machinery as every other message. No `JoinError` shape,
  no `select!` between joins and other work — if the coord also
  needs to handle a shutdown signal, that's just another variant in
  `CoordMsg`.
- **Final `Report` via `stop_with`.** The host gets one typed value
  back; no mpsc, no atomics.

What feels worse:

- **No "wait for child to boot before next step" primitive.** The
  Tokio side has `JoinSet::join_next` which both confirms a task
  *finished* and gives the result. The Tina side has
  `observe_isolate_complete(addr)` for one address at a time, but
  the coord doesn't *have* the children's addresses
  (`spawn(...)` does not give them back). We rely on the children's
  send to publish the result. That's correct — but if a worker
  panicked before sending, the coordinator would hang waiting for
  the missing partial. A per-spawn `observe_child_complete(...)`
  would let the coord notice missing partials as a typed
  `WorkerStopped` rather than a deadlock.
- **Mailbox sizing for the coord must account for every child's
  `WorkerDone` reply.** The `incoming + replies` rule (the mailbox-capacity
  rule) bites here: with 4 workers, the coord's mailbox holds
  `Start + 4 × WorkerDone = 5` outstanding, plus headroom. Easy to
  miscount under pressure (more workers, more reply slots).

What got better:

- **Self-address at registration time.** The old chicken-and-egg
  `Begin { self_addr }` bootstrap is gone; the coord now learns
  its own address through the
  `register_root_using(cap, |self_addr| ...)`
  constructor closure. The host kicks the work with a typed
  `CoordMsg::Start` that carries no address. (FINDINGS finding 3,
  self-address at registration.)
- **Application-shaped lifetime.** The host uses `LocalSystem` and its
  consuming `run_to_shutdown_reported` path, so early workload errors cannot bypass
  typed terminal validation.

## Partial-failure flavor

This specimen exercises the happy path: every worker finishes and
sends. `specimen_cancellation_chain` and the sharded scatter/gather
report from the earlier scatter/gather specimens cover the typed partial-aggregate shape. A
future variant of this specimen could:

- give one worker a slice that triggers a panic;
- have the coordinator join child completion and result publication as
  one parent-owned typed outcome, rather than relying on the result message
  alone;
- emit a `Report` with `results_collected < expected`.

Spawn addresses are already surfaced by the runtime. The remaining gap is a
parent-side child join/completion primitive that combines child termination
with expected result publication; `register_root_using` already resolves the
coordinator self-address side.
