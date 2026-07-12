# specimen_bounded_batcher

Many callers submit one item and wait on the original request. The batcher
replies to every caller with the batch total when either the batch reaches
`BATCH_SIZE` or `BATCH_TIMEOUT_MS` elapses from its first item.

## Tina shape

- `SharedWork<generation, BatcherReply>` owns the bounded set of callers for
  each batch. The batch generation is the real domain key; there is no
  synthetic request id or parallel reply-order sidecar.
- `RequestCall` and the linear permit returned by `SharedWork::wait` keep
  caller authority explicit. A saturated table returns that authority so the
  batcher can reply `Full` immediately.
- `reply_all_clone(generation, Batched(total))` settles the whole batch in
  FIFO admission order and frees its capacity for the next batch.
- `sleep(interval).then_service_event(...)` delivers a typed timer event. The
  generation makes an old timer harmless when a size flush wins the race. A
  current timer failure settles the batch with typed `TimerFailed(CallError)`
  replies instead of abandoning callers.
- The live host uses fallible `LocalSystem` startup, typed
  `call_blocking_request`, exhaustive inner call and outer host-control
  outcomes, and bounded truthful shutdown observation.

Caller timeout does not retract submitted work. `SharedWork` reclaims the
closed reply slot at the next admission, while the accepted item remains in
its batch. This is intentional: cancellation of a caller's wait is not
cancellation of an already accepted submission.

## Verification

The Tina tests directly cover size and timer flushes, pending-cap `Full`, a
timed-out caller followed by capacity refill, exact reclamation and rejection
counters, post-`Full` refill, stale successful/failed timer invalidation, timer
failure settlement and refill, and bounded clean shutdown. The smoke test runs
both the Tokio and Tina implementations with a producer statically below both
declared submission caps.

```sh
cargo test --manifest-path examples/specimen_bounded_batcher/Cargo.toml --all-targets
```
