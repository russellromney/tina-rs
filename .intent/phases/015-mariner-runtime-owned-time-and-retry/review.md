# 015 Reviews And Decisions

Session:

- A (artifacts)

## Plan Review 1

Artifacts prepared:

- `.intent/phases/015-mariner-runtime-owned-time-and-retry/spec-diff.md`
- `.intent/phases/015-mariner-runtime-owned-time-and-retry/plan.md`

Initial framing choices already pinned in the artifacts:

- 015 is a bigger coherent package, not a tiny "add `Sleep`" slice.
- The package proves runtime-owned time through a retry/backoff workload, not
  just a primitive unit test.
- The timer surface is intentionally narrow:
  - one-shot
  - relative
  - no cancellation
  - no repeating timers
  - no absolute wall-clock deadlines
- `tina` stays substrate-neutral; any clock-driving seam should stay
  crate-private to `tina-runtime-current`.
- Direct proof is required at two levels:
  - primitive timer semantics
  - user-shaped retry/backoff behavior
- The plan now answers the five Session A questions explicitly:
  - what we are building
  - what will not change
  - how we will build it
  - how we will prove it works
  - how we will prove earlier intent still holds
- The plan now names proof modes directly:
  - unit
  - integration
  - e2e
  - adversarial
  - blast-radius
- The plan now distinguishes direct proof from surrogate proof so later review
  can ask the IDD question plainly:
  "How could this still be broken while the current tests pass?"

Open questions for review:

- Is a crate-private deterministic clock seam the right proof shape for 015,
  or should the package prove the live timer path more directly even if tests
  are somewhat slower?
- Should the retry/backoff workload remain test-only, or also grow a runnable
  example in this same package?
- Is the existing trace vocabulary sufficient, or does timer work need one new
  event kind to stay observable without smuggling proof through internal state?

## Plan Review 2

Follow-up tightening after rereading the updated IDD guidance:

- The package now pins shipped runtime time semantics explicitly instead of
  leaving them as an implementation choice:
  - production `CurrentRuntime` owns a monotonic clock
  - each `step()` samples that clock once at step start
  - due timers are harvested against that sampled instant
  - tests may drive a crate-private manual clock only if it preserves that
    shipped meaning
- The package now also pins equal-deadline ordering:
  - same-due-time timers wake in deterministic request order
  - direct proof must cover both different-deadline ordering and equal-
    deadline tie-break behavior

These were real plan gaps, not just wording nits: without them, an
implementation could pass the old plan while still leaving shipped time
semantics fuzzy or replay ordering unstable.

## Implementation Review 1

Implementation landed with the planned runtime shape:

- `CallRequest::Sleep { after }` and `CallResult::TimerFired`
- monotonic runtime-owned clock sampled once at the start of each `step()`
- due-timer harvest inside `CurrentRuntime`
- deterministic equal-deadline tie-break in request order
- crate-private `ManualClock` seam for deterministic timer semantics tests

Direct proof now exists at the required levels:

- crate-local direct timer semantics tests prove:
  - single wake
  - no early wake
  - fires once
  - different-deadline ordering
  - equal-deadline request-order tie-break
  - stopped-requester rejection
- crate-local retry/backoff workload now performs a real retry:
  - first `Attempt` fails
  - runtime-owned `Sleep` delays retry
  - translated wake produces a second real `Attempt`
  - second attempt succeeds
- public-path integration proof in `tina-runtime-current/tests/retry_backoff.rs`
  drives the shipped runtime surface with the production monotonic clock and
  proves the same delayed-retry behavior without relying on the crate-private
  manual clock seam

Blast-radius proof remains green:

- `cargo +nightly test -p tina-runtime-current --test retry_backoff -- --nocapture`
- `cargo +nightly test -p tina-runtime-current --lib retry_backoff_workload_uses_timer_path -- --nocapture`
- `make verify`

The earlier gaps called out during implementation review are closed:

- runtime time-progression semantics are now explicit in the artifacts and in
  code
- equal-deadline ordering is pinned and directly proved
- the retry/backoff workload is now a real retry rather than "timer then
  success"
- call-vocabulary docs no longer describe timers as future work
