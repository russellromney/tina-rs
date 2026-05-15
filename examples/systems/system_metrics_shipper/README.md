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
  late `Submit` returns `Stopping` and `stop_clean` is true.

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
- The single-flight flush is implemented as `flush_in_flight: bool`. It
  is a one-bit `PendingReplies`. If another system also wants "one
  outstanding call to a fixed peer," that should grow a helper rather
  than living as a field on each isolate.
  - Planned fix: the bounded-lanes admission vocabulary in ROADMAP.md
    `Mailbox-first devex polish sketch`
    (`try_admit` / `permit` / `snapshot` / typed `Busy`). Specializes
    cleanly to N=1 and replaces the bool with a counter-shaped report.
- `Sink` needs three message shapes (`Flush` call, `Complete`
  continuation, `Stats` call) only because the slow sink wants a
  `defer(sleep)` path that produces an internal message variant.
  - Planned fix: ROADMAP.md `Mailbox-first devex polish sketch`
    proposes `call.reply_after(work).to_self(|reply, result| ...)` as
    the blessed sugar over `ctx.take_request_context().unwrap()`
    plus `reply_with_request`. The continuation message still exists
    (no hidden state mutation), but the call site collapses to one
    fluent line and `into_request_context` stops being a discoverability
    cliff.
- "Events lost on flush" is real but feels too cheap. A real shipper
  would want to re-enqueue the batch on a `Full` or `Closed` sink and
  retry once. The specimen punts on retry because it is out of scope,
  but the typed counter makes the gap visible.
  - Planned fix: ROADMAP.md `Backpressure policies` (explicit policy
    objects for shed, bounded wait, retry with backoff, degrade, close,
    each returning typed outcomes). Retry-once-on-`Full` is one of the
    named policies.
- Drain is hand-rolled choreography: set `draining = true`, stash the
  `Stop` `RequestContext`, let the next `FlushDone` reply. Works, but
  the order is now load-bearing across three handlers.
  - Planned fix: ROADMAP.md `Shutdown orchestration graph` (ordered
    helpers for stop ingress, cancel/close pools, drain in-flight work,
    flush batchers, terminal report). This specimen's `Stop` handler
    implements three of those five steps by hand.

Not yet on the roadmap:
- `Arc::try_unwrap(runtime)` for shutdown silently fails if any caller
  thread still holds an Arc clone. Both reference specimens use this
  shape. A `runtime.request_shutdown(timeout)` that did not depend on
  no-other-owners would be friendlier and would compose with the
  shutdown orchestration graph above.

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
- One new finding worth recording: a `request_shutdown` runtime API
  that does not require sole `Arc` ownership.

Verdict:
- keep
