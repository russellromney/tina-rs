# Phase 101 - Mailbox-First Service Ergonomics

Status: Planned. Build after the current system-specimen PRs settle, or in
parallel with protocol work if the branch stays mostly in `tina` /
`tina-runtime`.

## Grug Truth

System specimens are now hitting the same pain in different clothes:

- `system_metrics_shipper`: stale tick tokens, single-flight flush bool, manual
  drain.
- `system_job_queue`: bootstrap message, cancel + pending-handle pairing,
  child lifecycle pain.
- `system_session_auth`: recurring sweep bootstrap, sharded host-call gap,
  startup effects.
- `system_lock_manager`: lease-expiry tokens, FIFO wait queues, stale handles.
- `system_bounded_object_lane`: bounded in-flight admission and deferred reply
  ceremony.

This phase should remove mechanical ceremony. It must not hide Tina truth.
Continuation messages stay visible. State mutation stays in handlers. Full /
Closed / Timeout stay typed outcomes. Cancellation still means "Tina stopped
waiting" unless Tina owns the rail and proves stronger cancel.

## Goal

Make common long-lived service code easier for humans and LLMs to write
correctly:

- defer one runtime-owned work item and carry caller authority into the next
  mailbox turn;
- run recurring ticks with a named missed-tick policy;
- cap in-flight local work with a permit instead of hand-rolled bools;
- shut down services through a small explicit drain helper;
- start services that need startup effects without a forgotten host
  `try_send(Bootstrap)`;
- update system specimens to prove the helpers are copyable.

This phase may ship as two PRs if needed:

1. low-risk helper rocks: deferred-work docs/API polish, recurring ticks,
   local permits, drain state;
2. startup hook only if its design is clean enough after review.

If Rock 5 is not clean, leave it as a design note and do not block the rest of
the phase.

## Non-Goals

- No fake async/await surface.
- No hidden callbacks that mutate user state.
- No hidden retries.
- No hidden queues.
- No helper that says external work was cancelled when only the wait was
  cancelled.
- No broad event/request split here. Phase 100 owns that bigger model change.
- No broad bridge framework. Bridge convention audit owns that.

## API Homes

Do not scatter helpers.

- `tina::time`: timer decision/state types only.
- `tina-runtime`: runtime-effect builders, local permits, drain helpers, tests.
- `tina`: only tiny trait-surface additions needed by `CallContext`.
- examples/specimens: policy-heavy shapes that are not proven twice.

If a helper needs both `tina` and `tina-runtime`, prefer the smallest trait hook
in `tina` and the concrete implementation in `tina-runtime`.

## Rock 0 - Evidence Sweep

Before coding, read the current merged versions and any still-open PRs for:

- `examples/systems/system_metrics_shipper`
- `examples/systems/system_job_queue`
- `examples/systems/system_session_auth`
- `examples/systems/system_lock_manager`
- `examples/systems/system_bounded_object_lane`
- `examples/systems/system_cache_with_fill`
- `examples/systems/ergonomics_playground`

Write a short status block at the top of this plan with:

- which rough shapes repeated at least twice;
- which helpers are allowed to ship;
- which rough shapes stay specimen-local.

Also record which open PRs were already merged. Do not design against stale
branch copies.

## Rock 1 - Deferred Work to Self

First improve the copied non-cancelable shape from `handle_call`.

Today the safe shape is roughly:

```rust
call.defer(sleep(work)).reply(|request, outcome| Msg::Done {
    request,
    outcome,
})
```

That is honest, but users still have to know the builder vocabulary and often
fall back to older `reply_with_request` forms.

Ship one blessed spelling if it is actually clearer. Candidate:

```rust
call.defer(work).to_self(|reply, outcome| Msg::Done { reply, outcome })
```

or keep `.reply(...)` and improve docs/examples if the shipped API is already
the best name.

Hard rules:

- the helper returns an ordinary `Effect<I>`;
- the continuation message carries a `RequestContext`;
- the later handler must call `reply_to_request`;
- the helper does not auto-reply;
- the helper does not run user mutation inside the translator;
- error messages and docs point users away from deprecated
  `reply_with_request`.

Proof:

- one unit test for a deferred sleep from `handle_call`;
- one compile-fail or doc-fail proving the continuation has to carry the
  request context;
- one test that the deferred helper does not auto-reply; the caller only
  completes when the continuation handler calls `reply_to_request`;
- migrate one system specimen.

## Rock 2 - Recurring Work and Missed Ticks

`TimerInterval` exists. The missing piece is a service-shaped recurring loop
that is boring to copy.

Build a small helper/pattern for recurring ticks:

```rust
RecurringTick::every(period)
    .missed(MissedTickPolicy::Skip)
    .next(ctx.now())
```

Exact naming may differ. Keep the helper stateful and explicit.

Required policies:

- `Skip`: one late tick produces at most one visible skipped decision;
- `CatchUpBounded(n)`: catch up at most `n` ticks, never loop forever;
- `Delay`: schedule next tick from the current time.

Hard rules:

- no background thread;
- no ambient clock;
- helper only computes the next delay / decision;
- service still returns `sleep(delay).then(Msg::Tick)`;
- virtual time and live time use the same visible decision shape.
- stale ticks must be detected by explicit token/ordinal/deadline state, not by
  guessing from wall-clock timing.

Proof:

- unit tests for each missed-tick policy;
- stale tick after size-triggered flush is ignored visibly;
- catch-up cap is honored after a large time jump;
- simulator test for deterministic recurring ticks;
- migrate `system_metrics_shipper` or `specimen_periodic_batcher`.

## Rock 3 - Local In-Flight Permit

Several specimens use one bool or one counter to mean "work admitted but not
settled." Make that shape boring.

Candidate:

```rust
if let Some(permit) = self.in_flight.try_admit() {
    call.defer(work).to_self(move |reply, result| Msg::Done {
        permit,
        reply,
        result,
    })
} else {
    call.reply(Reply::Busy(self.in_flight.snapshot()))
}
```

This is not a pool. This is isolate-local admission.

Helper requirements:

- fixed count capacity;
- optional name for capacity/discovery reports;
- `Permit` is move-only and carries generation/id;
- release/retire exactly once through the helper;
- late/double release is visible in tests;
- snapshot/report says capacity, current, full_count, high_water.
- `Drop` must not silently release unless the design proves that cannot hide
  still-running work. Preferred first form: explicit release only; drain can
  retire outstanding permits and report them.

Proof:

- fill-refuse-release-refill;
- stale permit from before close/drain cannot release a newer generation;
- double release is rejected or reported;
- shutdown drains or reports outstanding permits;
- migrate `system_bounded_object_lane` or `system_metrics_shipper`.

## Rock 4 - Graceful Drain Helper

Specimens keep hand-rolling:

1. stop new admission;
2. finish or cancel in-flight work;
3. flush buffered work;
4. reply to parked callers;
5. stop with final report.

Build one tiny helper if it can stay explicit.

Candidate:

```rust
self.drain.begin();
self.pending.drain_into_effect(&mut effects, Reply::Closed);
self.in_flight.close();
effects.push(stop_with(report));
Effect::Batch(effects)
```

This may land as docs + small `DrainState`, not a mega helper.

Hard rules:

- no hidden ordering;
- no hidden resource close;
- every parked caller gets a typed terminal reply;
- outstanding work is either allowed to finish or visibly cancelled;
- late completion after drain is rejected/tombstoned visibly or routed to a
  terminal report; it must not reopen admission or leak a permit;
- final report names admitted/completed/cancelled/dropped/full.

Proof:

- stop while idle;
- stop while pending callers are parked;
- stop while one in-flight operation exists;
- new request during drain returns `Closed` / `Stopping`;
- late completion after drain is visible and does not mutate closed state;
- migrate `system_metrics_shipper` or `system_job_queue`.

## Rock 5 - Startup Effects

`system_job_queue` and `system_session_auth` both need a host-sent
`Bootstrap` only to start child/tick effects. Forgetting it makes a quiet
service.

Design and ship one small startup hook if it fits current runtime shape.

Candidate:

```rust
fn on_start(&mut self, ctx: &mut Context<'_, S, R>) -> Effect<Self> {
    ...
}
```

or:

```rust
register_started_with_capacity(isolate, cap)
```

Questions to pin:

- Does startup run after mailbox/address registration succeeds?
- What happens if startup returns `Stop`?
- What happens if startup panics?
- Does restart run startup again?
- Is startup trace-visible?
- Does simulator do the same thing?
- Is startup allowed to send/call/spawn before the isolate has processed any
  mailbox message?
- What address/generation does startup observe?

Hard rules:

- no address escapes from failed registration;
- startup effect is trace-visible;
- restart behavior is explicit;
- no hidden unbounded startup queue.
- if constructor succeeds but startup fails, the terminal state is typed and
  observable.

Proof:

- startup schedules first tick;
- startup spawns children;
- startup panic / stop has clear terminal truth;
- live and sim match for the same small case.

If any of these cannot be proven without a larger runtime model change, do not
ship startup hooks in this phase. Leave the design note and keep Bootstrap.

## Rock 6 - Backpressure Policy Objects

Only build this if the evidence sweep shows two real call sites with the same
policy. One retry-on-Full case is not enough.

Small policy objects may help with repeated `Full` handling:

```rust
RetryPolicy::bounded(max_attempts, Backoff::constant(...))
ShedPolicy::reply(...)
ClosePolicy::close(...)
```

Hard rules:

- caller chooses idempotency;
- attempts are capped;
- sleeps are Tina timers;
- every retry attempt is visible;
- no default retry.

Proof:

- one retry-on-Full specimen path;
- one shed-on-Full specimen path;
- report separates first-attempt `Full`, retry success, retry exhausted.

If the two call sites are not truly the same shape, leave policy in specimens.

## Rock 7 - Docs and Specimen Migrations

Update:

- `docs/tina-user-guide/04-request-reply.md`
- `docs/tina-user-guide/10-service-patterns.md`
- `docs/tina-user-guide/11-ergonomics-checklist.md`
- `docs/tina-user-guide/14-lifecycle-and-shutdown.md`
- `examples/FINDINGS.md`
- relevant system specimen READMEs.

Migrate at least two system specimens. Preferred:

- `system_metrics_shipper` for recurring ticks, single-flight, drain;
- `system_bounded_object_lane` for local permit and deferred work;
- `system_session_auth` for startup tick hook if Rock 5 ships.

Do not rewrite every specimen by force. Migrate the ones that prove the helper.

## Required Checks

Run focused checks for touched crates/specimens:

- `cargo fmt --all --check`
- `cargo test -p tina`
- `cargo test -p tina-runtime`
- `cargo test -p tina-sim` if startup/timer sim behavior changes
- touched system specimen `cargo test --manifest-path ...`
- touched specimen smoke tests
- `cargo clippy` for touched crates/specimens with `-D warnings`
- doc tests / compile-fail tests for any new public helper docs.

If a live test fails twice, treat it as a bug and inspect the code/logs. Do not
rerun until green by luck.
