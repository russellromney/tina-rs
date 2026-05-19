# Phase 118: Admission And Rate Policy

## Status

- Future implementation plan for Wave A.
- Runs after Phase 115 lands so public policy names live in the right layer.
- Can run in parallel with phases 116 and 117 if ownership stays in policy
  types, edge-service specimens, and docs.
- Builds on existing `SharedCapacityScope`, `LocalPermitGate`,
  `FullHandling`, `Backoff`, `RecurringTick`, capacity summaries, and service
  pressure reports. Do not rebuild those primitives.

## Starting Facts

- `LocalPermitGate` already gives fixed-count local concurrency with move-only
  permits and pressure reports.
- `SharedCapacityScope` already gives shared weighted shard-local budgets.
- `FullHandling` already gives `shed` and explicit retry-with-backoff decisions.
- `Backoff` and `RecurringTick` already use caller-owned time and are replayable.
- System findings say broad retry frameworks are a trap. Keep retry explicit and
  caller-owned.
- Current systems still hand-roll "gateway accepted / tenant full / retry later"
  language. This phase makes that copied path boring.
- `BoundedEventSink` now exists. Use it for policy/audit events only when the
  user explicitly asks for event retention; do not make it part of admission
  itself.

## Purpose

Give services boring pressure policy objects.

The user story:

```text
when I am overloaded, I choose shed, wait boundedly, rate-limit, degrade, or
close, and the outcome is typed
```

## Includes

- copied-path `ConcurrencyLimit` wrapper over `LocalPermitGate`
- `KeyedLimit<K>` for per-key/per-user caps with fixed-capacity storage
- `RateLimit<K>` with replayable time source using `Context::now`
- rate limiting policy is deterministic: same config + same visible time + same
  key history produces same decisions
- bounded-wait decision shape; caller still owns the waiting message/request
- if a bounded wait stores caller authority, storage must be fixed-capacity and
  return the request/context on rejection
- shed/degrade/close policy outcomes with one shared report shape
- retry-with-backoff policy that is explicit, bounded, and caller-owned
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
- no automatic idempotency decision; caller decides if retry is safe

## Blast Radius

Medium blast radius.

- Allowed: new policy types, docs, specimens, capacity/service-report adapters.
- Allowed only if needed: tiny helper methods on existing `LocalPermitGate`,
  `SharedCapacityScope`, `FullHandling`, `Backoff`, or `RecurringTick`.
- Not allowed: runtime scheduler changes, hidden queues inside runtime, new
  global registry, or rewriting existing capacity vocabulary.

## Implementation Shape

Create one user-facing admission vocabulary. Names should describe what the user
is doing, not the storage under it.

```text
ConcurrencyLimit
KeyedLimit<K>
RateLimit<K>
AdmissionDecision<T>
AdmissionFailure
AdmissionReport
PressureAction
```

`AdmissionDecision<T>` is the copied match shape:

```text
Admitted(T)
Full(AdmissionReport)
RateLimited { retry_after, report }
Wait { delay, report }
Degrade { report }
Closed(AdmissionReport)
TimedOut(AdmissionReport)
```

Rules:

- A successful decision returns a move-only permit/charge when capacity must be
  released later.
- Per-key storage is fixed-capacity. No growing `HashMap` as the user-visible
  storage truth.
- Duplicate-key and full-key-table paths are typed and include the report.
- Key eviction, if implemented, is explicit policy. Default first form does not
  evict a key silently to make room for a new key.
- Rate decisions take `now` from `ctx.now()` or simulator-supplied time. No
  `Instant::now()` inside the policy.
- Bounded wait returns a decision. It does not hide a queue unless the queue is
  fixed-capacity and returns the caller/request on rejection.
- Bounded wait uses the RequestContext / PendingCancelable patterns from the
  current runtime. No parked caller authority without a stored token and a
  fill-cancel-refill proof.
- Retry returns a sleep duration/token. It does not resend the request.
- Shared-budget use returns a `SharedCapacityCharge` or a typed full report.
- Reports convert into existing `CapacitySummary` / discovery lines.

## User Proof Specimens

- `system_api_gateway_limits`: two routes share one weighted budget for in-flight
  requests and body bytes. One route fills the budget; the other sees typed
  `Full` and the report names the shared surface.
- `system_tenant_rate_limiter`: one hot tenant is rate limited while a cold
  tenant still succeeds. Retry-after values are deterministic under sim time.
- `system_webhook_relay` or another outbound-edge system adopts explicit retry
  budget and shows idempotency named in the message, not inferred by helper.
- `specimen_rate_limited_worker` moves from hand-rolled rate state to
  `RateLimit`/`AdmissionDecision`.
- A tiny service shows `FullHandling` with bounded retry and explicit
  idempotency in the message name.

## Proof Shape

- each policy returns typed `Admitted` / `Full` / `RateLimited` / `Closed` /
  `TimedOut` style outcomes
- per-key cap cannot be bypassed by a different message path
- bounded wait reclaims capacity on cancel/timeout/shutdown; fill-cancel-refill
  must admit new work
- duplicate key / stale waiter / cancelled waiter cannot remove a newer entry
  with the same key; include a generation/ticket proof
- retry budget exhaustion is visible
- sim replay proves time-based policy determinism
- system specimens show edge/API-gateway and tenant limiting under pressure
- cold tenant/key progress is proved while hot tenant/key is limited
- compile-fail tests prove move-only permits cannot be released twice or reused
  after move
- compile-fail or API-shape tests prove a retry helper cannot run without an
  explicit idempotency/retry policy value
- reports show current/high/full/retry/degrade/closed counts and can be asserted
  through `CapacitySummary`
- shutdown while waiters/retry timers exist returns caller authority or typed
  terminal outcomes and leaves current counts at zero

## Hostile Review Notes

- Do not invent a second capacity product. This is a user-facing policy layer
  over the existing pressure/capacity reports.
- Do not build "helpful retry." Helpful retry lies unless the caller named the
  operation idempotent.
- Do not call an internal storage object a "slab" in public API. Users want
  limits, permits, decisions, and reports.
- Do not let per-tenant limiters become unbounded maps.
- Do not make live-only rate timing. Sim and live must make the same decision
  from the same visible time/config.
- Do not hide overload in logs. The caller gets a typed outcome.
