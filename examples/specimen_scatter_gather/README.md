# specimen_scatter_gather

A coordinator service that receives client queries, fans each query
out to N workers in parallel, gathers the per-worker results, and
replies to the original client with the aggregate. Workers reply
out of order. Tokio uses `tokio::spawn` + `mpsc` + `oneshot`. Tina
uses one `Coordinator` isolate with `PendingReplies` +
`DeferredReply` and an `Effect::Batch` of runtime calls per query.

## Run

```sh
cargo run --manifest-path examples/specimen_scatter_gather/Cargo.toml -- both
cargo run --manifest-path examples/specimen_scatter_gather/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_scatter_gather/Cargo.toml -- tina
```

Both sides report:

```
side=tokio clients=6 workers=4 ok=6 wrong=0 failed=0 exit_clean=true
side=tina  clients=6 workers=4 ok=6 wrong=0 failed=0 exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs) — workers as tasks behind
  `mpsc`, coordinator task that spawns one fan-out sub-task per
  query, oneshot per query for the aggregate.
- [`src/tina_impl.rs`](src/tina_impl.rs) — `Coordinator` isolate with
  `PendingReplies<u64, AggregateReply>::with_capacity(MAX_IN_FLIGHT)`,
  one `DeferredReply` captured per query, `Effect::Batch` of N
  runtime calls per fan-out, gather via `WorkerDone` continuations.

## Tokio shape

The coordinator owns one `mpsc::Receiver<CoordReq>`. For each
admitted query it spawns a sub-task that fans out to every worker
and joins the per-worker oneshots. The query's own `oneshot::Sender`
travels with the request, so there is no `HashMap<RequestId,
oneshot::Sender>` — a small win against the most obvious anti-pattern.

What is *not* bounded:

- Spawned sub-tasks per query — there is no named cap. The
  coordinator's `mpsc` inbox bounds incoming queries, but once a
  query is admitted the sub-task lives until `join_all` finishes.
- Total parallelism is whatever the Tokio runtime + FD limits + per-
  worker `mpsc(8)` happen to allow. None of those numbers appear in
  the coordinator's API.

## Tina shape

The coordinator captures the caller as a deferred slot via
`PendingReplies::try_capture(ctx, qid)`. On admission failure
(`Full`) it answers the caller immediately with
`reply(AggregateReply::Full)` *without* consuming the caller —
the slot ceremony is conditional on capacity. The successful
aggregate is `AggregateReply::Ok(sum)`.

Each admitted query becomes:

```rust
Effect::Batch(workers.iter().map(|w| {
    Effect::Call(RuntimeCall::isolate_call(
        *w,
        WorkerMsg::Do(payload),
        QUERY_TIMEOUT,
        move |outcome| CoordMsg::WorkerDone(qid, outcome),
    ))
}).collect())
```

Worker replies arrive interleaved across queries. `WorkerDone(qid,
outcome)` looks up the partial entry, accumulates the sum, and when
the last per-query reply lands:

```rust
let slot = self.pending.take(&qid).unwrap();
return reply_to(slot, AggregateReply::Ok(done.sum));
```

What is bounded by name:

- `MAX_IN_FLIGHT` is the named cap on captured callers.
- Each worker isolate has its own mailbox capacity.
- Each runtime call has an explicit `QUERY_TIMEOUT`.

## Discussion

What feels different:

- **The cap has a name.** Tokio's bound is "however many sub-tasks
  the runtime can hold up", which is fine until it isn't. Tina's
  pending box is `MAX_IN_FLIGHT`; admission past the cap turns into
  a typed `Full` reply the caller sees.
- **Slot ceremony rides with the policy decision.** Tokio splits
  "should we admit this?" (mpsc backpressure) from "where does the
  reply go?" (oneshot in the request). They happen to compose, but
  there is no single place that says "I am storing this caller's
  promise." Tina's `try_capture` makes that explicit and fails
  visibly when the box is full.
- **Out-of-order is the same on both sides.** Tokio handles it via
  per-query oneshots and `join_all`; Tina handles it via
  `qid` correlation in `WorkerDone`. Same idea, different shapes.

What Tina costs you here:

- **More message types.** `WorkerMsg`, `CoordMsg`, `DriverMsg` plus
  three reply types are explicit. Tokio's
  `Vec<oneshot::Receiver<u64>>` skips the message-type ceremony.
- **The `qid` is yours to manage.** Tokio's per-query oneshot lives
  on the query's stack frame. Tina's per-query state lives in the
  isolate, keyed by an integer you assigned. Plenty of room for
  off-by-one if the partial-state vec and the pending box drift.

What 061 closed:

- **No `Arc<Mutex<HashMap<RequestId, OneShot>>>`.** The pending box
  is a single named container with a hard cap, sweep, and visible
  counters (`high_water`, `full_rejects`, `reclaimed`,
  `duplicate_keys`).
- **Caller liveness is a runtime fact.** A timed-out caller's slot
  closes with a terminal `DeferredReplyRejected{CallerClosed}`
  trace event before the next admission check sweeps it.
