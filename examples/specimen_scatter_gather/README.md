# specimen_scatter_gather

A coordinator receives concurrent client queries, fans each one out to four
workers, retains exhaustive per-worker outcomes, and replies to the original
caller. Tokio uses spawned tasks, channels, and per-query oneshots. Tina uses a
bounded `ScatterGatherOperations` owner.

## Run

```sh
cargo run --manifest-path examples/specimen_scatter_gather/Cargo.toml -- both
cargo run --manifest-path examples/specimen_scatter_gather/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_scatter_gather/Cargo.toml -- tina
```

Both sides report:

```text
side=tokio clients=6 workers=4 ok=6 wrong=0 failed=0 exit_clean=true
side=tina  clients=6 workers=4 ok=6 wrong=0 failed=0 exit_clean=true
```

Read [`src/tokio_impl.rs`](src/tokio_impl.rs) and
[`src/tina_impl.rs`](src/tina_impl.rs) top to bottom.

## Tokio shape

The coordinator owns one bounded `mpsc::Receiver`. Each admitted query spawns
a task that sends to every worker and joins per-worker oneshots. The query's
reply oneshot travels with it, so there is no global request-id map.

The inbox is bounded, but admitted fanout tasks and their total parallelism do
not have a named cap in the coordinator's state.

## Tina shape

The coordinator owns the worker addresses and one bounded operation owner:

```rust
struct Coordinator {
    workers: Vec<Address<WorkerMsg, WorkerReply>>,
    operations: ScatterGatherOperations<usize, WorkerReply, AggregateReply>,
}
```

Each request supplies a bounded target list and typed cancelable call factory:

```rust
self.operations.start_service(
    request,
    config,
    targets,
    move |worker, timeout| call_cancelable(worker, WorkerMsg::Do(payload), timeout),
    CoordEvent::Scatter,
)
```

Replies, aggregate expiry, and cancellation settlement all use the same event
variant:

```rust
let advance = self.operations.advance_service(event, CoordEvent::Scatter)?;
if let Some(done) = advance.completed {
    reply_to(done.request, AggregateReply::Complete(done.report))
}
```

`MAX_IN_FLIGHT` bounds concurrent aggregates. `ScatterGatherConfig` bounds
targets and names per-target and aggregate deadlines. Each operation owns the
original `RequestContext`, child cancellation authority, caller target order,
and terminal rows. Admission past the operation cap returns the untouched
caller and becomes `AggregateReply::Full`.

The completed report keeps `Replied`, `Full`, `Closed`, `Timeout`, `Rejected`,
`AggregateTimeout`, and `MissingShard` distinct. Only the driver decides
whether those exhaustive rows form a successful aggregate.

## Discussion

What improves over manual coordination:

- There is no application qid, pending-reply table, partial-row vector, or
  lookup/removal protocol to keep synchronized.
- Opaque operation and branch tokens route out-of-order replies and reject
  stale continuations.
- Aggregate expiry marks only unfinished rows and withholds caller authority
  until emitted cancellation work settles.
- A capacity-one live test proves excess callers receive typed `Full`, the
  retired slot admits a refill, and no caller remains stranded at shutdown.

Tina still spells domain messages and reply types that Tokio can leave inside
task-local channel types. In return, the concurrency cap, authority ownership,
timeouts, cancellation, and terminal outcomes are all visible in the service
contract.

The coordinator API is backend-neutral. The repository parity suite runs this
same `start_service` / `advance_service` authoring form on the live, threaded,
multi-shard, and simulator owners; this specimen stays focused on the live
application path instead of duplicating that matrix.
