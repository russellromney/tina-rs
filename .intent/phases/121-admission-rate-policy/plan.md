# Phase 121: Admission And Rate Policy

## Status

- Future IDD outline for Wave B.
- Can run in parallel with phases 120 and 122 if ownership stays in policy
  types, edge-service specimens, and docs.

## Purpose

Give services boring pressure policy objects.

The user story:

```text
when I am overloaded, I choose shed, wait boundedly, rate-limit, degrade, or
close, and the outcome is typed
```

## Includes

- concurrency limiter
- per-key/per-user limiter
- rate limiter with replayable time source
- bounded-wait policy
- shed/degrade/close policy outcomes
- retry-with-backoff policy that is explicit and bounded
- service report/capacity integration
- API gateway limits system specimen
- tenant rate limiter system specimen

## Does Not Include

- no hidden retry
- no invisible queue
- no probabilistic policy without deterministic seed/config
- no global admission registry

## Proof Shape

- each policy returns typed `Admitted` / `Full` / `RateLimited` / `Closed` /
  `TimedOut` style outcomes
- per-key cap cannot be bypassed by a different message path
- bounded wait reclaims capacity on cancel/timeout/shutdown
- retry budget exhaustion is visible
- sim replay proves time-based policy determinism
- compile-fail tests catch wrong clock/config typestate where practical

