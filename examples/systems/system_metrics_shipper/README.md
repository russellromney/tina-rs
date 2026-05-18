# Metrics Shipper

A tiny periodic batcher. One isolate accepts metric events, batches them
by size or time, and single-flights each batch through a downstream sink
(the HTTP/DB stand-in). Shutdown drains pending events with one more
flush before replying.

## Architecture

```text
host callers --(call Submit)--> Shipper isolate
                                  |
                                  | call(sink, Flush, timeout).then(FlushDone)
                                  v
                                Sink isolate (HTTP/DB stand-in)
```

The shipper owns request order, batch buffer, single-flight flush, and
the drain handshake. The sink owns "what reached the wire," including a
configurable per-flush delay that simulates a slow upstream.

## Capacity

Every queue, pool, and timer has a cap. Every cap is in the
`Run{Steady,Overload,Shutdown}Report` produced by `run(...)`.

| Surface | Cap | Where reported |
| --- | --- | --- |
| Shipper mailbox | `shipper_mailbox` | `shipper_mailbox_full` on the report |
| In-memory buffer | `buffer_capacity` | `buffer_high_water`, `buffer_full_rejects` |
| Batch flush trigger (size) | `batch_size` | `batches_flushed_by_size` |
| Batch flush trigger (time) | `batch_window_ms` | `batches_flushed_by_time`, `ticks_armed`, `ticks_fired_useful`, `ticks_fired_stale`, `ticks_fired_idle` |
| Drain flush trigger | (shutdown handshake) | `batches_flushed_on_drain`, `flushed_on_drain`, `drained_batches` |
| Single-flight sink call | one in flight | `flush_failures`, `events_lost_on_flush` |
| Sink mailbox | `sink_mailbox` | `SinkStats.mailbox_capacity` |

A typed `ShipperReply` distinguishes the three ingress outcomes:
`Accepted`, `Dropped` (buffer full), and `Stopping` (drain in progress).
Shipper-mailbox backpressure surfaces as `CallOutcome::Full` to the
host, separately counted as `shipper_mailbox_full`.

## Flush Triggers

- **Size:** `Submit` lands and the buffer is now at or above `batch_size`
  while no flush is in flight. `armed_tick` advances so the sleeping
  timer becomes stale; that fire is counted in `ticks_fired_stale`.
- **Time:** the armed tick fires, the buffer is non-empty, and no flush
  is in flight. Counted in `ticks_fired_useful`.
- **Drain:** `Stop` arrives. New `Submit` calls receive `Stopping` and
  any remaining events ride a single drain flush. The `Stop` reply is
  held in a `RequestContext` until the drain `FlushDone` arrives.

## Run

```sh
cargo run --manifest-path examples/systems/system_metrics_shipper/Cargo.toml
```

## Smoke

```sh
cargo test --manifest-path examples/systems/system_metrics_shipper/Cargo.toml
```

The smoke test runs three scripted scenarios and asserts the typed
fields, not log text:

- **steady** — every accepted event reaches the sink; sink batch count
  equals `by_size + by_time + on_drain`; buffer high-water never exceeds
  the cap.
- **overload** — parallel callers plus a slow sink force the buffer to
  overflow; the number of typed `Dropped` replies equals
  `buffer_full_rejects`; no event silently disappears between accepted
  and routed.
- **shutdown** — a partial batch (smaller than `batch_size`) plus `Stop`
  produces one drain flush; every drained event lands at the sink; a
  late `Submit` returns `Stopping` and `stop_clean` is true. The same
  scenario also exercises the typed lifecycle helpers: the report
  carries a `ServiceTopology` (shipper / sink / flush_tick), a typed
  `Health` snapshot in [`Lifecycle::Stopped`], and a
  `ServiceShutdownReport` that records `DrainInFlight` →
  `CloseResource sink.isolate` → `StopOwner` in order. This is the
  worked non-HTTP example for [`tina_runtime::lifecycle`]; the helper
  must not become HTTP-shaped by accident.

## Out Of Scope

No real HTTP client, no real DB connection, no retry, no jittered
backoff, no per-domain rate limit, no sampling. Those move in once a
later specimen (delivery daemon, webhook relay) hits the same rough
bits and earns a real primitive.

## Findings

What felt good:
- `SingleShard` + bounded mailbox + `CallContext::reply` makes the
  three-way ingress outcome (`Accepted` / `Dropped` / `Stopping`)
  trivial to encode and assert without log scraping.
- `sleep(window).then(Tick { token })` plus a hand-rolled token discipline
  is enough to keep size-triggered flushes from racing the time-window
  timer. Stale ticks are counted, not silently dropped.
- `call(sink, Flush, timeout).then(FlushDone)` gives the shipper one
  outstanding downstream call with a typed `CallOutcome` covering
  `Replied`, `Full`, `Closed`, and `Timeout`.
- `Stop` is a single deferred reply: stash the `RequestContext`, let the
  drain `FlushDone` answer it. No new primitive required.

What felt rough (with the planned roadmap row that already names the fix):
- Tick token bookkeeping is hand-rolled. Three independent paths must
  agree to invalidate `armed_tick`: a size-triggered flush, a drain
  flush, and the `FlushDone` path that may re-arm. The token is
  load-bearing here in a way it is not in
  `ergonomics_playground::debounced_batch`, because that probe's only
  non-timer state change clears the buffer, so a stale fire harmlessly
  flushes nothing; size-triggered flushes leave a non-empty buffer, so
  a stale fire would double-flush without the token.
  - Planned fix: ROADMAP.md `Runtime-owned recurring work` (cron/
    periodic patterns with **missed-tick policy**) and `Timer
    vocabulary` (periodic service patterns beyond the current debounce/
    throttle helper state). This specimen's hand-rolled token *is* a
    missed-tick policy.
- The single-flight flush now uses `tina_runtime::LocalPermitGate` with
  capacity 1, named `"flush"`. The pressure invariant ("one in-flight
  flush") is structural and the gate's report (current, capacity,
  full_count, high_water, retired_count) is one line.
- `Sink` continues to need three message shapes (`Flush` call, `Complete`
  continuation, `Stats` call) for the slow sink, but the call site now
  uses `call.defer(sleep(...)).reply(|req, _| SinkMsg::Complete { req, batch })`
  with no `into_request_context()` cliff in sight.
- "Events lost on flush" is still real. A real shipper would want a
  retry-once-on-`Full` policy via `tina_runtime::FullHandling`; the
  helper now exists and the typed counter makes the gap visible.
- Drain is now state plus tiny helpers: `DrainState::begin()` flips
  admission, `flush_tick.clear()` invalidates pending timer
  continuations, the `Stop` handler stashes its `RequestContext`, and
  the next `FlushDone` calls `DrainState::finish()` and answers via
  `reply_to_request`. Ordering is visible, no hidden close.

Closed by Phase 106 (lifecycle, health, topology):
- The host shutdown sequence now runs through
  `tina_runtime::lifecycle::ShutdownChoreography`. Each step (`DrainInFlight`
  for the shipper's own Stop handshake, `CloseResource sink.isolate`,
  `StopOwner` for the runtime) is typed, time-stamped, and visible in the
  terminal `ServiceShutdownReport`. The choreography's ordering-violation
  detection caught the first attempt to record a post-stop ingress
  invariant as a `StopIngress` phase; that check is now an assertion
  rather than a misplaced choreography step. `lifecycle_for_drain_stage`
  maps `DrainStage::{Open,Draining,Stopped}` to
  `Lifecycle::{Ready,Draining,Stopped}` so a non-HTTP service reports
  state in the same words as `mini_saas_api`.

Closed by Phase 102 (host-control ergonomics):
- `Arc::try_unwrap(runtime)` is gone from this specimen. The host now
  takes a cloneable `ThreadedShutdownHandle` from
  `runtime.shutdown_handle()`, calls `request_shutdown()` (nonblocking,
  idempotent, fails fast with `ShutdownRequestError::CommandFull`
  rather than hanging), and waits on the cached terminal report via
  `wait_report(timeout)`. The handle controls **runtime** shutdown;
  the shipper's own `Stop`/`DrainState` protocol still owns
  service-level drain.

Related shapes:
- `ergonomics_playground::debounced_batch` parks each submitter in
  `PendingReplies` and replies a single batched value when the timer
  flushes; this specimen replies `Accepted` synchronously and pipelines
  events to a downstream sink. Same timer + buffer + drain skeleton,
  different "who waits." The blessed
  `PendingReplies::drain_replies_with_into_effect` helper only fits the
  parked-caller shape.

Tina capability pulled:
- Bounded mailboxes with typed `Full` at the host boundary.
- `CallContext` with `defer(work).reply(...)` and `into_request_context`.
- Runtime-owned timers via `sleep(...).then(...)`.
- `call(addr, msg, timeout).then(...)` for single-flight downstream
  fan-out with a typed `CallOutcome`.
- `reply_to_request` for deferred replies held across a continuation.

Suggested follow-up:
- The roadmap already names the four primitives that would erase the
  rough bits above (missed-tick policy, admission vocabulary, mailbox-
  first defer sugar, backpressure policies, shutdown orchestration).
  This specimen is one piece of evidence those rows are pulling on real
  pain, not speculative ergonomics.

Verdict:
- keep
