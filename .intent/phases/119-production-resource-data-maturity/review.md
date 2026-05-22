# 119 Implementation Findings

Findings from building the resource half (2026-05-22). The durable half
was deferred to Phase 126 to avoid a colliding second design of the same
names; see plan Status.

## Finding 1 — Idle age is stamped on first sweep, not at release

`WorkerPoolMsg::Release` carries no `now`, so the pool cannot record the
exact instant a resource went idle. Threading `now` into every release
(and its `release_effect` / `release_result_effect` sugar) would touch a
lot of call sites — pool tests, keepalive, examples — for a small gain.

Chosen shape: a resource that returns to idle clears its `idle_since`,
and the first `Maintain { now }` sweep that observes it idle stamps the
clock. Idle age therefore runs from the first post-release sweep, so the
maintenance cadence bounds idle granularity. The fix is to run
maintenance at least as often as `max_idle`; this is documented on
`ResourceLifetime` and in `docs/resource-owner-matrix.md`.

If a future phase wants release-accurate idle age, the clean move is to
add `now` to the release path once, not to special-case it here.

## Finding 2 — A generic pool cannot health-check `H`; only the caller and the sweep can

`ResourceHealth` and `PolicyCheckPoint` name three check points
(`BeforeHandoff`, `AfterRelease`, `ScheduledMaintenance`), but the
generic `WorkerPool` can only realize two of them: the caller's
release-time verdict (`AfterRelease`, via `ReleaseDisposition::Retire`)
and age in the maintenance sweep (`ScheduledMaintenance`). A
before-handoff health probe needs to inspect the concrete resource, which
the generic pool over an opaque `H` cannot do.

`BeforeHandoff` is kept in the vocabulary because owner-specific pools
(e.g. a DB bridge that can cheaply ping a connection before handing it
out) can produce it. This is a deliberate capability boundary, not an
oversight: the pool reports; the owner probes.

## Finding 3 — Max-lifetime is enforced at sweep granularity, not at the handoff instant

`WorkerPoolMsg::Acquire` carries no `now` either, so the pool cannot
re-check max age at the exact moment of handoff. Between sweeps a
just-over-age idle resource could still be handed out. This matches the
plan's "explicit maintenance messages" model — staleness is bounded by
the sweep interval, not zero — and keeps the hot acquire path unchanged.
The proof (`max_lifetime_retire_does_not_hand_stale_to_new_caller`) runs
a sweep before the next acquire, which is the documented owner pattern.

## Finding 4 — Retiring a generic slot shrinks capacity until the owner refills

The generic pool is built over a fixed handle list, so retiring a slot
(idle age, max age, or caller `Retire`) drops capacity by one until the
owner hands a fresh handle back via `Refill`. This is correct — the pool
cannot build an `H` — but it means "fill-retire-refill" is a two-party
dance: the pool reports the dead slot, the owner closes the real resource
and refills. Keepalive sidesteps this entirely: its idle retirement
closes the *socket* and keeps the slot leasable (the connection isolate
reconnects on next request), because the keepalive resource is the
self-healing isolate, not the transport.

## Self-review fixes (caught after first pass)

A deep re-read of the state machine caught two issues, both fixed with
regression tests:

- **Refill reset the generation counter to 1.** The pool's whole
  stale-lease / double-release defense keys on monotonic generations.
  Refilling a slot at generation 1 reused old generation values, which
  would let a stale low-generation lease alias the reborn slot if the API
  ever gained a way to retire a leased slot (it does not today, and a
  closed pool refuses refill, so it was not yet exploitable — but it
  diverged from the codebase's bar). Fix: `ResourceState::Retired` now
  carries `next_generation`, and refill continues the counter. Proof:
  `refill_keeps_generation_monotonic`.
- **`Maintain` had no closed-pool guard.** A maintenance tick racing a
  close would retire idle slots — including the ones force-close
  deliberately keeps in `Idle` state so a stray late release still
  reports `DoubleRelease`. Fix: `Maintain` on a closed pool reports
  current shape and retires nothing; close owns shutdown semantics
  (matching `Refill`, which already refused a closed pool). Proof:
  `maintain_on_closed_pool_retires_nothing`.
