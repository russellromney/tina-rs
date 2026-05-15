# Phase 101 - Mailbox-First Service Ergonomics

Status: Ready to implement.

This is not a planning phase. The choices are pinned here. Build the helpers
below, migrate the named specimens, and prove the behavior.

## Locked Choices

Ship in this phase:

- keep `CallContext::defer(work).reply(...)` as the blessed non-cancelable
  multi-turn reply spelling;
- add a small recurring-tick service helper on top of existing
  `TimerInterval`;
- add a small isolate-local in-flight permit helper;
- add a small explicit drain-state helper;
- add a register-and-bootstrap helper that enqueues one explicit startup
  message after registration;
- add small explicit Full-handling policy state for shed/retry-with-backoff;
- update docs and the named specimen/system call sites below.

Do not ship in this phase:

- startup hooks / `on_start`;
- hidden async/await-like callbacks;
- hidden retries;
- hidden resource close.

Startup hooks are real future work, but not here. The current Tina truth stays:
startup work begins from an explicit mailbox message such as `Bootstrap`. This
phase makes that harder to forget by adding a helper that registers and enqueues
the bootstrap message in one call.

Backpressure policy in this phase is tiny and explicit: choose `Shed` or
`RetryBackoff`, return a typed decision, and let the service schedule the sleep
or reply. No helper resends a message by itself.

## Grug Truth

System specimens keep hitting the same ceremony:

- one delayed runtime work item must carry caller authority to a later mailbox
  turn;
- periodic work needs a missed-tick rule;
- local "work in flight" bools/counters need one boring helper;
- graceful shutdown needs the same stop-admit / drain / report shape;
- startup bootstrap messages are easy to forget;
- retry-on-Full code repeats the same budget/backoff ceremony.

Remove that ceremony. Do not hide Tina truth.

Continuation messages stay visible. State mutation stays in handlers. Full /
Closed / Timeout stay typed outcomes. Cancellation still means "Tina stopped
waiting" unless Tina owns the rail and proves stronger cancel.

## API Homes

Do not scatter helpers.

- `tina::time`: timer decision/state types.
- `tina-runtime`: runtime-effect builders, register-bootstrap helpers,
  `LocalPermitGate`, `DrainState`, `FullHandling`, tests.
- `tina`: only tiny trait hooks needed by caller authority.
- examples/specimens: policy-heavy shapes that are not proven twice.

If a helper needs both `tina` and `tina-runtime`, prefer the smallest trait hook
in `tina` and the concrete implementation in `tina-runtime`.

## Rock 0 - Confirm Inputs

Read current main before editing:

- `examples/systems/system_metrics_shipper`
- `examples/systems/system_bounded_object_lane`
- `examples/systems/system_job_queue`
- `examples/systems/system_session_auth`
- `examples/systems/ergonomics_playground`
- `tina/src/time.rs`
- `tina-runtime/src/single_call_gate.rs`
- `tina-runtime/src/call.rs`

This is only to avoid stale branch assumptions. Do not reopen scope.

Allowed implementation targets are Rocks 1-7.

## Rock 1 - Blessed Deferred Work Docs and Proofs

Do not rename the good path. The blessed spelling is:

```rust
call_ctx
    .defer(work)
    .reply(|request, outcome| Msg::Done { request, outcome })
```

The continuation message carries `RequestContext<R>`. The later handler answers
with `reply_to_request(request, value)`.

Implementation work:

- make docs and examples consistently use `CallContext::defer(...).reply(...)`;
- demote older raw `reply_with_request` examples to "escape hatch" text;
- improve rustdoc wording so the old shape does not look like the copied path;
- do not add a `to_self` alias in this phase. More names make learning worse.

Hard rules:

- helper returns an ordinary `Effect<I>`;
- no auto-reply;
- no hidden state mutation;
- no hidden callback that mutates user state;
- old call-site docs must not teach losing caller authority.

Proof:

- unit test: deferred sleep from `handle_call` returns only after the
  continuation handler calls `reply_to_request`;
- doc/compile-fail proof: `RequestContext` is move-only and cannot be answered
  twice;
- migrate one system specimen that still uses older wording or comments.

## Rock 2 - Recurring Tick Service Helper

Existing `TimerInterval` is useful but still too raw for services. Add one
small helper/state wrapper for service loops.

Add these public `tina::time` types:

- `RecurringTick`
- `RecurringTickDecision`
- `RecurringTickToken`
- `RecurringTickReport`

Use the existing `MissedTickPolicy`.

```rust
RecurringTick::every(period)
    .missed_tick_policy(MissedTickPolicy::Skip)
```

The helper must compute decisions only. The service still schedules the effect:

```rust
match self.flush_tick.next(ctx.now()) {
    RecurringTickDecision::Sleep(delay, token) => sleep(delay).then(Msg::FlushTick(token)),
    RecurringTickDecision::Skip(report) => ...
}
```

- `Skip`: coalesce missed ticks into one visible decision; never loop forever;
- `CatchUpBounded(n)`: allow at most `n` immediate catch-up ticks;
- `Delay`: schedule the next tick from the current observed time;
- stale ticks are detected by explicit token/ordinal/deadline state, not by
  wall-clock guessing.

Hard rules:

- no background thread;
- no ambient clock;
- no hidden self-send;
- no hidden work execution;
- live and sim expose the same decision shape.

Proof:

- unit tests for each missed-tick policy;
- stale tick after size-triggered flush is ignored visibly;
- large time jump honors the catch-up cap;
- simulator test proves deterministic recurring ticks;
- migrate `system_metrics_shipper`.

## Rock 3 - Isolate-Local In-Flight Permits

Several services use one bool/counter to mean "local work admitted but not
settled." Build `tina_runtime::LocalPermitGate`. This is not a pool.

Copied shape:

```rust
match self.in_flight.try_admit() {
    Ok(permit) => call_ctx.defer(work).reply(move |request, result| {
        Msg::Done { permit, request, result }
    }),
    Err(full) => call_ctx.reply(Reply::Busy(full.report())),
}
```

Helper requirements:

- fixed count capacity;
- optional static name for capacity/discovery reports;
- `Permit` is move-only and carries generation/id;
- release/retire exactly once through the gate;
- release/retire returns `Result<LocalPermitReport, LocalPermitReleaseError>`;
- stale/double release returns `LocalPermitReleaseError::StaleOrUnknown` and
  increments `invalid_release_count`;
- report type is `LocalPermitReport`;
- report says capacity, current, full_count, high_water, retired_count,
  invalid_release_count;
- `Drop` must not silently release. First form requires explicit release or
  explicit retire.

Hard rules:

- no hidden queue;
- no hidden retry;
- no pool behavior;
- no auto-release on drop.

Proof:

- fill-refuse-release-refill;
- stale permit from before close/drain cannot release a newer generation;
- double release returns `LocalPermitReleaseError::StaleOrUnknown` and leaves
  `current` unchanged;
- shutdown/drain reports outstanding permits;
- capacity report uses a valid token-like surface name;
- migrate `system_bounded_object_lane`.

## Rock 4 - Explicit Drain State

Build `tina_runtime::DrainState` for the common shutdown state:

1. stop new admission;
2. settle parked callers;
3. wait for or retire local permits;
4. flush final report;
5. stop.

Copied shape:

```rust
self.drain.begin();
self.pending.drain_into_effect(&mut effects, Reply::Closed);
self.in_flight.close_for_drain();
if self.drain.can_stop(self.in_flight.report()) {
    effects.push(stop_with(report));
}
Effect::Batch(effects)
```

This is small state plus docs. It must not become a shutdown framework.

Hard rules:

- no hidden ordering;
- no hidden resource close;
- every parked caller gets a typed terminal reply;
- outstanding work is either allowed to finish or visibly retired/cancelled;
- late completion after drain is visible and must not reopen admission;
- final report names admitted, completed, cancelled/retired, dropped, full.

Proof:

- stop while idle;
- stop while pending callers are parked;
- stop while one in-flight operation exists;
- new request during drain returns `Closed` / `Stopping`;
- late completion after drain is visible and does not mutate closed state;
- migrate `system_metrics_shipper`.

## Rock 5 - Register and Bootstrap

Do not add `on_start` yet. Add a helper that keeps startup as an ordinary
mailbox message but removes the host-side "register then remember to
`try_send(Bootstrap)`" footgun.

Build these helpers on the public registration surfaces:

```rust
runtime.register_with_capacity_and_bootstrap::<I, Outbound>(
    isolate,
    mailbox_capacity,
    bootstrap_msg,
)
```

and shard-aware mirror:

```rust
runtime.register_with_capacity_and_bootstrap_on::<I, Outbound>(
    shard,
    isolate,
    mailbox_capacity,
    bootstrap_msg,
)
```

Mirror the helper on `Runtime`, `ThreadedRuntime`, `MultiShardRuntime`, and
`ThreadedMultiShardRuntime` wherever the matching plain registration method
already exists.

Implementation rule:

- create the bounded mailbox;
- enqueue `bootstrap_msg` into that mailbox before inserting the isolate entry
  into the registry;
- insert the isolate entry only after bootstrap admission succeeds.

This is the load-bearing rule. Do not implement this as "register, then
`try_send`, then clean up if send fails." Cleanup after public registration is
where leaked addresses and tombstone confusion live.

Behavior:

- register the isolate normally;
- enqueue exactly one `bootstrap_msg` into its mailbox as part of registration;
- return the address only after the bootstrap message is admitted;
- bootstrap is just a normal message and normal trace-visible delivery;
- no special lifecycle callback;
- no hidden effect execution before the first mailbox turn;
- no retry if the bootstrap message cannot be admitted.

Failure rules:

- `mailbox_capacity == 0` is rejected by existing validation;
- because the mailbox is empty before registration, bootstrap admission
  succeeds for a valid default mailbox with capacity at least 1;
- if a custom mailbox rejects the bootstrap prefill, return a typed
  `RegisterBootstrapError` and do not insert the isolate entry;
- do not allocate an observable address before bootstrap admission succeeds;
- do not return an address for a service whose bootstrap was not admitted.

Immediate-send rule:

- the returned address may have a full mailbox until `Bootstrap` is delivered;
- this is honest pressure, not a bug. Document it.

Proof:

- helper registers and first delivered message is `Bootstrap`;
- no host `try_send(Bootstrap)` appears in migrated specimen setup;
- bootstrap can schedule first recurring tick;
- bootstrap can spawn children using normal handler code;
- live and sim/multishard mirrors behave the same where those registration
  surfaces exist;
- custom mailbox that rejects the prefill returns `RegisterBootstrapError`;
- failed prefill leaves no registered isolate and no returned address;
- sending immediately after helper returns can see `Full` if capacity is 1 and
  `Bootstrap` has not been delivered yet.

Migrate:

- `system_session_auth` recurring sweep bootstrap;
- `system_job_queue` child startup bootstrap.

## Rock 6 - Full Handling Policy State

Build tiny state for the repeated "on Full, shed or retry with backoff" shape.
This is policy state, not a retry engine.

Public shape:

```rust
let decision = self.full_policy.on_full(ctx.now(), self.deadline);
match decision {
    FullDecision::Shed(report) => call_ctx.reply(Reply::Busy(report)),
    FullDecision::RetryAfter(delay, report) => sleep(delay).then(Msg::Retry(report.token())),
    FullDecision::Exhausted(report) => call_ctx.reply(Reply::Busy(report)),
}
```

Required types:

- `FullHandling`
- `FullDecision`
- `FullHandlingReport`

Use existing `Backoff` for retry timing.

`FullHandling` is plain state. It does not own the mailbox, message, address,
or caller. It only records attempts and returns the next decision.

Required methods:

- `on_full(now, deadline) -> FullDecision`;
- `record_success() -> FullHandlingReport`;
- `reset()`.

Hard rules:

- no hidden resend;
- no hidden sleep;
- no default retry;
- caller chooses idempotency before using retry mode;
- attempts are capped;
- every retry delay is a Tina `sleep`;
- report separates first-attempt Full, retry Full, retry success, exhausted.

Proof:

- shed-on-Full returns one typed decision and no sleep;
- retry-on-Full returns bounded sleep decisions and then `Exhausted`;
- deadline elapsed returns `Exhausted` / `DeadlineElapsed` without scheduling;
- `record_success()` resets attempts and records retry success if a retry
  happened;
- migrated specimen/report shows first-attempt Full separately from retry Full.

Migrate one existing Full-retry path:

- `specimen_hot_key_fairness`.

## Rock 7 - Docs and Specimen Migrations

Update:

- `docs/tina-user-guide/04-request-reply.md`
- `docs/tina-user-guide/10-service-patterns.md`
- `docs/tina-user-guide/11-ergonomics-checklist.md`
- `docs/tina-user-guide/14-lifecycle-and-shutdown.md`
- `examples/FINDINGS.md`
- relevant system specimen READMEs.

Migrate exactly these two system specimens:

- `system_metrics_shipper` for recurring ticks, single-flight/permit, drain;
- `system_bounded_object_lane` for local permits and deferred work.

Also migrate both bootstrap specimens through Rock 5:

- `system_session_auth`;
- `system_job_queue`.

Also migrate `specimen_hot_key_fairness` through Rock 6.

Do not rewrite every specimen by force.

Docs must say plainly:

- use `CallContext::defer(...).reply(...)` for ordinary multi-turn replies;
- use explicit `Bootstrap` for startup work until a later startup phase;
- use register-and-bootstrap helper when a service always needs its first
  bootstrap message;
- use local permits for isolate-local admitted work;
- use pools only when the resource is actually leased across handlers.

## Required Checks

Run focused checks for touched crates/specimens:

- `cargo fmt --all --check`
- `cargo test -p tina`
- `cargo test -p tina-runtime`
- `cargo test -p tina-sim`
- touched system specimen `cargo test --manifest-path ...`
- touched specimen smoke tests
- `cargo clippy` for touched crates/specimens with `-D warnings`
- doc tests / compile-fail tests for new or changed public helper docs.

If a live test fails twice, treat it as a bug and inspect the code/logs. Do not
rerun until green by luck.
