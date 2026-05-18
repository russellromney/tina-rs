# Phase 118: Admission And Rate Policy

## Status

- Future IDD outline for Wave A.
- Can run in parallel with phases 116 and 117 if ownership stays in policy
  types, edge-service specimens, and docs.
- Builds on existing `SharedCapacityScope`, `LocalPermitGate`,
  `FullHandling`, `Backoff`, `RecurringTick`, capacity summaries, and service
  pressure reports. Do not rebuild those primitives.

## Purpose

Give services boring pressure policy objects.

The user story:

```text
when I am overloaded, I choose shed, wait boundedly, rate-limit, degrade, or
close, and the outcome is typed
```

## Includes

- copied-path concurrency limiter wrapper over `LocalPermitGate`
- per-key/per-user limiter with bounded key storage
- rate limiter with replayable time source using `Context::now`
- bounded-wait policy
- shed/degrade/close policy outcomes
- retry-with-backoff policy that is explicit and bounded
- service report/capacity integration
- composition with `SharedCapacityScope` for weighted shared budgets
- API gateway limits system specimen
- tenant rate limiter system specimen

## Does Not Include

- no hidden retry
- no invisible queue
- no probabilistic policy without deterministic seed/config
- no global admission registry
- no duplicate pressure vocabulary beside existing capacity/service reports
- no generic scheduler fairness work; Phase 121 owns fairness/load behavior

## Proof Shape

- each policy returns typed `Admitted` / `Full` / `RateLimited` / `Closed` /
  `TimedOut` style outcomes
- per-key cap cannot be bypassed by a different message path
- bounded wait reclaims capacity on cancel/timeout/shutdown
- retry budget exhaustion is visible
- sim replay proves time-based policy determinism
- system specimens show edge/API-gateway and tenant limiting under pressure
- compile-fail tests catch wrong clock/config typestate where practical
