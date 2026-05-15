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
This is not allowed to stay as "figure it out while implementing." The plan now
removes `on_start` from Phase 101 but still fixes the user footgun by shipping
`register_with_capacity_and_bootstrap(...)`: startup remains an ordinary
mailbox message, admitted before the address is returned.

## Finding 3 [P2] Permit drop semantics must be explicit

A local permit helper can silently lie if a dropped permit auto-releases when
work is still running, or leaks forever if it does not. The plan now requires
the implementation to choose and prove the behavior. Move-only is not enough.

## Finding 4 [P2] Drain helper can become a hidden shutdown framework

Graceful shutdown is service policy. A helper that closes resources in secret
would be anti-Tina. Rock 4 now frames the output as either small `DrainState` or
docs plus tiny helpers. The ordering stays visible.

## Finding 5 [P3] Backpressure policy can become fake retry magic

Retry-on-Full is policy, not mechanism. A broad retry framework would be fake
magic. The plan now ships only tiny `FullHandling` state: it returns
`Shed`/`RetryAfter`/`Exhausted`; the service still schedules the Tina sleep or
replies. No helper resends messages.

## Finding 6 [P3] Too many migrations can blur the proof

The phase could waste time rewriting the world. Rock 5 now asks for at least two
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

## Finding 9 [P2] Startup hook should not be in this implementation phase

Startup is useful but dangerous. Allowing "design-only startup" made this phase
partly a planning phase. The plan now says no lifecycle hook in 101. It ships
register-and-bootstrap instead, which is smaller and preserves mailbox truth.

## Finding 10 [P3] Required checks missed `tina` and doc tests

New public helper docs and trait hooks can fail in `tina` even when
`tina-runtime` is green. The required checks now include `cargo test -p tina`
and doc/compile-fail tests for new public helper docs.

## Finding 11 [P2] The plan still had "decide while coding" escape hatches

Phase plans should be executable, not a place to re-run product design. The plan
now locks the choices: no `to_self` alias, no `on_start`, exact bootstrap
helper, exact recurring/permit/drain helper names, and a tiny Full-handling
state instead of a retry framework.

## Finding 12 [P2] "Small" was hiding useful work

The previous revision dropped startup and Full-handling because they were
dangerous when vague. That made the phase smaller, but not better. The current
plan includes them in grug form: explicit bootstrap message admission and
decision-only Full handling. Bigger surface, less mush.
