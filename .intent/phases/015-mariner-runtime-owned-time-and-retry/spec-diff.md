# 015 Mariner Runtime-Owned Time And Retry

Session:

- A

## In Plain English

Phase 014 finished the first honest TCP server shape: the listener can keep
accepting, spawning, supervising, re-arming, and shutting down through the
normal runtime workflow.

The next missing Mariner capability is runtime-owned time.

Tina-Odin already has timers, and the broader Tina/DST story needs time to be
owned by the runtime rather than smuggled in through handler-side sleeps or
test harness delays. Without runtime-owned time, the current call contract
still risks looking like "TCP plus batching" rather than a general runtime
effect surface.

This package adds the smallest honest time capability:

- one-shot relative sleep / timer wake
- completion delivered later as an ordinary isolate `Message`
- no async handlers
- no hidden executor
- no wall-clock API at the `tina` trait boundary

But the package should not stop at a naked timer primitive. To prove that the
capability is real and worth keeping, 015 also adds a small retry/backoff
reference workload that uses runtime-owned time directly.

The point is not "we can wake up later." The point is "the runtime can own
delay, and user-visible retry/backoff behavior can stay inside Tina's normal
message/effect model."

## What Changes

- `tina-runtime-current` grows the first runtime-owned time call verb.
- The verb is one-shot and relative only:
  - "wake me after this duration"
  - not "wake me at this wall-clock timestamp"
- Completion still comes back through the existing call translator path as an
  ordinary later `Message`.
- The current runtime gains timer execution and wake delivery that are
  compatible with its existing synchronous `step()` model.
- In the shipped runtime, time progression is explicit and step-scoped:
  - `CurrentRuntime` samples a monotonic runtime-owned clock once at the
    start of each `step()`
  - any timer due at or before that sampled instant becomes eligible in that
    step
  - `Sleep { after }` therefore means "become eligible no earlier than
    `armed_at + after` on a future step"
- The runtime may introduce a crate-private clock / timer-driver seam if that
  is the cleanest way to prove timer behavior deterministically without
  depending on flaky wall-clock sleeps in tests.
- If multiple timers become due at the same sampled instant, wake order is
  deterministic in request order (the runtime's insertion / call-order tie
  break), not unspecified scheduler luck.
- A user-shaped retry/backoff proof workload lands alongside the primitive:
  - first attempt fails
  - runtime-owned timer delays retry
  - retry arrives through normal message flow
  - repeated runs remain deterministic under the same driven clock behavior

## What Does Not Change

- `tina` stays substrate-neutral.
- Handlers stay synchronous.
- `CurrentRuntime::step()` stays synchronous and caller-driven.
- This package does not introduce repeating timers, cron-like schedules,
  absolute deadlines, or cancellation.
- This package does not expose wall-clock access or `Instant`/`SystemTime`
  queries at the `tina` trait boundary.
- This package does not turn the runtime into a general scheduling framework.
- This package does not introduce simulator infrastructure yet, even if the
  timer implementation is shaped to leave a clean path to Voyager.
- This package does not touch mailbox crates, multi-shard work, Tokio bridge
  work, or benchmark claims.

## Acceptance

- A runtime-owned one-shot sleep/timer verb exists and is executable in
  `tina-runtime-current`.
- Timer completion is delivered as an ordinary later `Message` through the
  existing call translator path.
- The timer does not fire early.
- The timer fires once.
- Multiple timers with different delays wake in deterministic due-time order.
- Multiple timers with the same due time wake in deterministic request order.
- Stopped requesters reject late timer completion through the normal
  `CallCompletionRejected` path.
- The retry/backoff proof workload demonstrates:
  - an initial failed attempt
  - a delayed retry driven by runtime-owned time
  - success on a later attempt
  - no handler-side blocking or sleeping
- The proof surface includes both:
  - direct timer semantics tests
  - the user-shaped retry/backoff workload
- Trace assertions still matter:
  - call dispatch / completion / failure / rejection are asserted, not just
    final user state
  - the retry workload proves the delayed turn really came from the timer path
- The proof does not rely on fragile real-time sleeps if a deterministic
  runtime-driven clock seam is available.
- `make verify` passes.

## Notes

- This is intentionally bigger than "add `Sleep` and a unit test." The package
  should prove that runtime-owned time is a real part of Tina's model, not
  just a vocabulary stub.
- The preferred shape is still "ordinary messages and explicit effects," not
  a special timer callback model.
- Tests may drive a crate-private manual clock for deterministic proof, but
  that does not change the shipped runtime meaning: production
  `CurrentRuntime` still samples its monotonic clock at the start of each
  step.
