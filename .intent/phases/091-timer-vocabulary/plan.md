# Phase 091: Timer Vocabulary

## Status

- Ready to implement.
- One PR.
- Can run beside HTTP/2 or AWS follow-ups if it only touches timer helper
  vocabulary, docs, and specimens.

## Grug Truth

Time is runtime-owned.

Ambient clocks lie to replay.

Sleep exists.

Services need more than one sleep.

Intervals drift unless policy is named.

Retries lie unless jitter and max attempts are visible.

Debounce/throttle hide drops unless drops are counted.

No hidden background scheduler.

## Goal

Add small replay-safe timer helpers for common service loops:

- interval;
- backoff;
- retry delay;
- debounce;
- throttle.

They must compile into ordinary Tina effects:

```text
sleep(delay).reply(NextMessage)
```

The helper may calculate the next delay and name the state. It must not hide:

- the timer effect;
- the continuation message;
- missed ticks;
- dropped work;
- retry attempt count;
- jitter seed;
- deadline truth.

One PR. No framework.

First form target:

- ship interval and backoff;
- ship debounce/throttle only if they stay tiny;
- document explicit manual state when a helper would need a hidden queue.

## Non-Goals

- no async runtime;
- no task scheduler;
- no cron parser;
- no wall-clock calendar time;
- no ambient `Instant::now()` inside helper state;
- no hidden retry of user work;
- no automatic idempotency guess;
- no unbounded queue of pending ticks;
- no global timer registry.

## Rock 0: Read First

Read:

- `tina/src/lib.rs` around `sleep(...)`, `Context::now()`, and
  `Context::deadline_after(...)`;
- `tina-runtime` sleep/timer tests;
- `tina-sim` sleep handling;
- `examples/specimen_bounded_batcher`;
- `examples/specimen_rate_limited_worker`;
- retry/backoff code in bridge/specimen examples.

Write a short status note at the top before coding:

- API home;
- shipped helpers;
- specimen chosen.

Likely API home:

- pure helper state in `tina::time`;
- runtime-specific effect builders stay where `sleep(...)` already lives;
- no helper depends on live-only clocks.

Do not add a second timer subsystem. These helpers sit on the existing
runtime-owned sleep call.

## Rock 1: Interval

Ship a tiny interval state helper.

Candidate shape:

```rust
let mut interval = TimerInterval::every(period)
    .missed_tick_policy(MissedTickPolicy::Skip);

let delay = interval.next_delay(ctx.now());
return sleep(delay).reply(Msg::Tick);
```

Required policy:

- `Delay`: next tick is period after observed completion;
- `Skip`: skip missed ticks and report how many;
- `Burst`: allow catch-up, but bounded by `max_catch_up`.

First form may ship only one or two policies if the others start to sprawl.

Report must expose:

- tick number;
- scheduled time;
- actual observed time;
- missed count.

No helper emits work without an explicit `sleep(...).reply(...)` in user code
or a clearly named helper that returns that exact effect.

No unbounded zero-delay catch-up loop. If `Burst` catches up, it must have a
small cap and a report that says catch-up was exhausted.

## Rock 2: Backoff And Jitter

Ship backoff as pure data.

Candidate shape:

```rust
let mut backoff = Backoff::exponential(base, max)
    .with_max_attempts(5)
    .with_jitter(Jitter::seeded(seed, JitterRange::half()));

match backoff.next_delay() {
    Some(delay) => sleep(delay).reply(Msg::Retry { attempt }),
    None => reply(Failed),
}
```

Rules:

- max attempts is explicit;
- max delay is explicit;
- duration math is saturating or returns a typed overflow/config error;
- jitter seed is explicit for replay;
- no `thread_rng`;
- no hidden retry of the operation;
- report separates first failure, retry success, and exhausted attempts.

Jitter may be skipped in first form if deterministic seeded jitter is not small.
Do not add random jitter with ambient randomness.

## Rock 3: Debounce And Throttle

Ship only if small.

These are easy to lie with, so keep them blunt.

Debounce:

- latest event wins;
- older pending event count is visible;
- one timer in flight;
- no hidden queue;
- first form is one debounce stream, not a map of keys.

Throttle:

- allow one event per period;
- rejected/delayed/dropped policy is explicit;
- dropped count is visible;
- if `DelayedLatest` ships, it stores at most one latest event.

If the helper needs a queue, stop. Document the explicit state-machine pattern
instead.

## Rock 4: Deadline Integration

All helpers that wait must accept an optional `Deadline`.

Rules:

- delay is capped by remaining deadline when caller asks for it;
- deadline exhaustion returns typed `TimerDecision::DeadlineElapsed`;
- no live-only `Deadline::after()` shortcut;
- examples use `ctx.deadline_after(...)` or
  `Deadline::from_instant(ctx.now(), ...)`;
- helper APIs take `now` from the caller, usually `ctx.now()`;
  helpers do not sample a clock internally.

## Rock 5: Specimens

Update at least one existing specimen.

Good candidates:

- `specimen_bounded_batcher`: interval helper;
- `specimen_rate_limited_worker`: debounce/throttle or gate-plus-timer cleanup;
- retrying HTTP/DB specimen: backoff helper.

Do not rewrite every specimen.

The specimen README must show:

- old pain briefly;
- new copied shape;
- what remains explicit.

## Rock 6: Tests

Required tests:

- interval first tick and repeated tick math;
- missed tick policy;
- backoff caps at max delay;
- max attempts exhausts visibly;
- seeded jitter is deterministic, if jitter ships;
- deadline caps delay or reports elapsed;
- debounce/throttle visible drop/count behavior, if shipped;
- simulator replay proof for at least one helper-backed loop;
- no helper calls ambient wall-clock time;
- no unbounded zero-delay loop under catch-up.

Tests should be mostly pure unit tests plus one runtime/sim proof.

## Docs

Update:

- user guide timer section or service-patterns section;
- `09-tokio-to-tina-porting.md` if it mentions `tokio::time::interval`,
  `select!`, backoff, or debounce;
- the changed specimen README.

Use one copied pattern:

```text
helper decides delay
user returns sleep(delay).reply(...)
continuation updates helper state
```

## Required Checks

- `cargo fmt --all --check`
- `cargo test -p tina`
- `cargo test -p tina-runtime timer --tests`
- `cargo test -p tina-sim timer --tests`
- touched specimen smoke test
- `cargo clippy -p tina -p tina-runtime -p tina-sim --tests -- -D warnings`

## Hostile Review Notes

- Risk: a timer helper becomes fake async control flow.
  Fix: helper is state/math; the effect remains explicit.
- Risk: interval hides missed ticks.
  Fix: missed policy and missed count are first-form.
- Risk: jitter breaks replay.
  Fix: seeded jitter only, or no jitter in first form.
- Risk: debounce/throttle hides dropped work.
  Fix: visible drop/delay counters, no hidden queue.
- Risk: deadline uses live clock.
  Fix: all deadline math is anchored to `ctx.now()`.
- Risk: too many helpers in one PR.
  Fix: interval + backoff are enough. Debounce/throttle ship only if boring.
