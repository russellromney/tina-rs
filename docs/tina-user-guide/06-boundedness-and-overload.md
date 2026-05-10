# Boundedness And Overload

Tina should make overload boring and visible.

Important words:

- accepted
- full
- closed
- timeout

If a system is under pressure, it should shed load explicitly instead of
quietly growing hidden queues.

## Mailbox Capacity

Every isolate mailbox has capacity. The capacity is one number, but
it has to absorb two distinct streams of messages:

> total capacity = inbound messages + replies to outstanding runtime calls

If an isolate has 4 inbound senders and runs 2 `tcp_read`s in flight
at once, both peaks have to fit. A capacity of `4` will reject the
read replies under load with `CallCompletionRejected { reason:
MailboxFull }` in the trace.

Use [`MailboxBudget`](https://docs.rs/tina-runtime) at the spawn site
to make the math obvious:

```rust
use tina_runtime::MailboxBudget;

let cap = MailboxBudget::session(
    /* inbound peer messages */ 4,
    /* in-flight runtime calls */ 2,
).total();

spawn(ChildDefinition::new(child, cap))
```

Presets for common shapes:

- `MailboxBudget::listener(max_in_flight_accepts)` — listener that
  re-arms one `tcp_accept` per ready peer.
- `MailboxBudget::session(in_flight_peer_messages, in_flight_runtime_calls)`
  — TCP/HTTP session isolate.
- `MailboxBudget::service(concurrent_requests, in_flight_runtime_calls)`
  — request/reply service.
- `MailboxBudget::fanout(upstream_callers, worker_count)` — frontend
  that fans out to N workers; each worker reply needs a slot.

If capacity is `1`, the second queued message should hit pressure.

## Pressure Diagnostics

`Runtime::pressure_summary()` and `ThreadedRuntime::pressure_summary()`
walk the trace and return a counted summary:

```rust
let summary = runtime.pressure_summary()?;
assert_eq!(summary.completion_rejected_mailbox_full, 0);
println!("pressure: {summary}");
// pressure: completion[mbox_full=0 ...] reply[no_pending=0 ...] send[full=0 closed=0]
```

`summary.any_full()` is the one-line "did we hit a *Full* anywhere?"
check. The closed/no-pending counts reflect lifecycle, not capacity,
so they're broken out separately.

## Observed Send

Plain `send` is simple but does not tell sender what happened.

Use observed send when overload matters:

```rust
use tina_runtime::{send_observed, SendOutcome};

#[derive(Debug, Clone)]
enum ProducerMsg {
    Burst(usize),
    Sent(SendOutcome),
}

#[tina_runtime::isolate(
    message = ProducerMsg,
    send = Outbound<ConsumerMsg>,
    shard = AppShard
)]
impl Producer {
    fn handle(&mut self, msg: ProducerMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ProducerMsg::Burst(n) => batch(
                (0..n)
                    .map(|i| {
                        send_observed(self.consumer, ConsumerMsg::Item(i))
                            .reply(ProducerMsg::Sent)
                    })
                    .collect(),
            ),
            ProducerMsg::Sent(outcome) => {
                if outcome.is_full() {
                    self.rejected += 1;
                }
                noop()
            }
        }
    }
}
```

This is the heart of Tina service shape.

Many async systems accept into a channel or spawned task until pressure appears
somewhere else.

Tina should be able to say:

```text
accepted=12000 full=38000 timeouts=0 exit=clean
```

## Bounded Pools

When the bounded thing is "borrow one of N resources, do work, give
it back," reach for `tina_runtime::pool::WorkerPool`. It bounds two
quantities — resource count (`PoolConfig::capacity`) and parked
waiters (`PoolConfig::max_waiters`) — and surfaces overflow as the
typed outcomes above (`AcquireOutcome::Full` /
`AcquireOutcome::Closed`). Caller cancellation reclaims waiter
capacity; the pressure report exposes `full_count`, `cancel_count`,
`closed_count`, `retired_count`, and `dispatch_recovered` for
operator dashboards. Don't open-code a worker frontend with a
`PendingReplies` table when you actually want a borrow/return
lifecycle. See the [ergonomics
checklist](./11-ergonomics-checklist.md#bounded-worker-pool).

## Timeout Is Load Control

Request/reply uses timeout:

```rust
call(worker, WorkerMsg::Run(job), Duration::from_millis(20))
    .reply(ClientMsg::Done)
```

Handle timeout as normal behavior.

Do not panic on timeout in service code.

## Measurement Rule

When testing a Tina service under load, collect:

- accepted work
- rejected full
- closed
- timeouts
- peak pending if easy
- exit status
- crude RSS if the platform gives it

First pass can run without hard memory caps. Later pressure passes can use
Linux/Fly/Docker limits.

## Capacity Is Not A Guess

Bounded queues need numbers. Picking those numbers is a workflow:

```text
unknown -> measured -> fixed
```

Don't write `usize::MAX`. Don't write 10_000. Pick a number, mark it
**Tuning**, run the workload, read **high water**, freeze a **Fixed**
cap with a small safety factor.

Vocabulary lives in `tina::capacity`:

- `CapacityMode::Fixed` — a measured cap. Production-acceptable.
- `CapacityMode::Tuning` — a discovery cap. **Still a hard upper
  bound.** The flag only tells the report to surface high water
  loudly so you know what to freeze it at.
- `CapacityPolicy::{Development, Test, Production}` — gates which
  modes are allowed. Both modes pass everywhere today; weight,
  shared scopes, and unbounded escape hatches land in a later phase.
- `CapacitySurfaceReport` — `{ name, mode, max_messages, current,
  high_water, full_count, ... }`. Every count surface produces one.

Two count surfaces report today:

- `WorkerPool` waiters — surface name defaults to
  `pool.<id>.waiters`. Pin an explicit name with `.named(...)` when
  CI / DST tests assert on this surface.
- `PendingReplies` slots — surface name defaults to
  `pending_replies.<n>`. Same `.named(...)` story.

`tina-runtime::CapacitySummary` collects reports and offers
`Result`-flavored assertions:

```rust
use tina_runtime::{CapacitySummary, format_discovery_line};
use tina::capacity::CapacityMode;

let mut summary = CapacitySummary::new();
summary.push(report)?;

summary.surface("orders.mailbox").no_full()?;
summary.surface("pool.demo.waiters").high_water_at_most(96)?;
```

`format_discovery_line(&report)` prints one boring `key=value`-ish
line per surface with a next-action hint:

```text
pool.demo.waiters     tuning  max=4    cur=0   high=4   full=0   suggest=tuning cap is tight; raise then re-measure
orders.mailbox        fixed   max=64   cur=0   high=11  full=0   suggest=fixed cap is loose; consider shrinking
```

The hint is advice, not a metric. Read it, decide, freeze the cap.

`PoolPressureReport::to_waiters_capacity_report(name, mode)` and
`PendingReplies::capacity_report()` are the two adapters that emit
the generic shape today; new bounded surfaces will follow the same
pattern.

## What Counts As Failure

Good failure:

```text
server says full
client gets response
process stays alive
metrics make sense
```

Bad failure:

```text
process OOMs
latency goes strange
no overload signal exists
shutdown hangs
metrics lie
```

When Tina fails badly, write it down. That is the work.
