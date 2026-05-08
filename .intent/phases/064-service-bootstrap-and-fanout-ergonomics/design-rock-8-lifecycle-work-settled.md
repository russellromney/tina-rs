# Rock 8 Design Note — Lifecycle / Work-Settled Observation

## Status

Design only. No helper. Driver-isolate `stop_with(report)` is
the blessed pattern.

## What "Settled" Means

A host that wants "the app is done" really wants one of:

- one isolate stopped — `observe_isolate_complete(addr)` ships;
- one isolate produced a typed value — `observe_result::<T>(addr)`
  + `stop_with(value)` ships;
- producer stopped, consumer caught up — make one isolate emit
  a typed "consumer reached final position" message and stop
  with it;
- all admitted work drained — the frontend that owns the
  `PendingReplies` knows when empty and stops with that fact;
- signal observed, no in-flight timer — single-isolate fact.

Pattern across all: name the thing that finishes, make one
isolate own it, return a typed value via `stop_with`, observe
that one isolate.

## Why No `runtime.wait_idle()`

A vague `wait_idle()` would have to define which mailboxes,
which calls, and which timers count. Bridges have their own
drain. `tcp_bind` listeners are intentionally never idle.
Restart-pending children may be in flight. Each edge is an
*application* fact, not a runtime fact. A single helper that
papered over them would lie.

The blessed shape keeps the driver isolate in user code:

```rust
let report_waiter = runtime.observe_result::<AppReport, _, _>(driver)?;
runtime.try_send(driver, DriverMsg::Begin)?;
let report = report_waiter.wait(timeout)?;
```

The driver decides when settled is true. The host learns it
through one shipped primitive.

## Specimens Already On This Pattern

- `eiffel_graceful_pool_shutdown` — driver counts and stops
  with `DriverOutcome`.
- `eiffel_dynamic_worker_pool` — coord stops with `Report`.
- `eiffel_webhook_publisher` — `observe_isolate_complete` on
  the driver.
- `eiffel_outbound_http` — scripting driver stops with typed
  outcome.

If a future specimen wants "settled across multiple isolates",
write one driver isolate that subscribes to the others and
stops with the aggregate. Stays inside the model.

## What Could Ship Later

A test-only `HostBarrier` for example tests where a driver
isolate is overkill. Plan calls this out as test plumbing, not
the blessed app shape. Scoped tightly:

- live-only;
- test plumbing only;
- not exported from runtime crate root;
- never named `wait_idle` or `wait_for_settled`.

The public runtime API does not gain a "settled" predicate in
this phase.

## Decision

No helper. Document the pattern in the user guide's lifecycle
chapter:

> A driver isolate that owns the app's terminal condition and
> finishes with `stop_with(report)` is the honest shape. The
> host observes one address through `observe_result::<T>` and
> gets one typed value back. There is no runtime-wide "idle"
> or "settled" predicate, on purpose.

Specimens already migrated stay as is.
