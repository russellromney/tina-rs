# Hostile Review - Phase 101

## Finding 1 [P2] Rock 1 can rename an already-good API

`CallContext::defer(work).reply(...)` already exists. The plan originally risked
building naming churn instead of solving a real bug. Keep Rock 1 honest: inspect
the current API first. Ship a new name only if it removes repeated confusion in
merged specimens. Docs/examples may be the right output.

## Finding 2 [P2] Startup hooks can break registration atomicity

Startup effects are attractive, but this touches the same dangerous area as
self-address registration: address allocation, constructor failure, panic,
restart, and trace order. The plan must require explicit answers before code.
Rock 5 now pins these questions and allows design-only success if the hook is
not safe yet.

## Finding 3 [P2] Permit drop semantics must be explicit

A local permit helper can silently lie if a dropped permit auto-releases when
work is still running, or leaks forever if it does not. The plan now requires
the implementation to choose and prove the behavior. Move-only is not enough.

## Finding 4 [P2] Drain helper can become a hidden shutdown framework

Graceful shutdown is service policy. A helper that closes resources in secret
would be anti-Tina. Rock 4 now frames the output as either small `DrainState` or
docs plus tiny helpers. The ordering stays visible.

## Finding 5 [P3] Backpressure policy can become fake retry magic

Retry-on-Full is policy, not mechanism. The plan now gates policy objects on at
least two real call sites and requires caller-owned idempotency, capped attempts,
and visible Tina sleeps.

## Finding 6 [P3] Too many migrations can blur the proof

The phase could waste time rewriting the world. Rock 7 now asks for at least two
targeted system migrations and explicitly says not to force every specimen.

## Finding 7 [P2] API homes were too loose

The first plan said the branch should stay mostly in `tina` / `tina-runtime`,
but did not pin where helpers belong. That invites one helper in `tina`, one in
`tina-runtime`, and one specimen-local copy. The plan now names homes:
`tina::time` for timer state, `tina-runtime` for concrete runtime/service
helpers, `tina` only for tiny trait hooks, examples for policy-heavy shapes.

## Finding 8 [P2] Missed-tick semantics were fuzzy

`Skip` originally said "if work already happened," which is not an
implementation rule. The plan now requires explicit token/ordinal/deadline
state, stale-tick proof after size-triggered flush, and bounded catch-up after a
large time jump.

## Finding 9 [P2] Startup hook should be allowed to fail design review

Startup is useful but dangerous. The plan now splits the phase into low-risk
helpers plus startup hook only if registration/restart truth survives review.
Design-only startup is a valid outcome.

## Finding 10 [P3] Required checks missed `tina` and doc tests

New public helper docs and trait hooks can fail in `tina` even when
`tina-runtime` is green. The required checks now include `cargo test -p tina`
and doc/compile-fail tests for new public helper docs.
