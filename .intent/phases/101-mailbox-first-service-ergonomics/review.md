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

