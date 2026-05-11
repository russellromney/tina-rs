# 085 Race / Join Helpers

## Status

- Done: plan created after 072 `PendingCallSet`, 079 cancellation round
  2, and the 084 child-lifecycle plan. 081 request context should land
  first. 084 should land first if this phase races/joins child work.
- In progress: none.
- Open: implement bounded explicit race/join helpers and update one or
  two specimens.
- Deferred: macros, fake `select!`, hidden retry, stream select,
  arbitrary policy framework, heterogeneous-reply race helpers.

## Goal

Give Tina an honest equivalent for the useful parts of:

```text
select!
join!
JoinSet
```

Not syntax cosplay. The Tina version should keep names, caps, deadlines,
cancel outcomes, and partial results visible.

Grug truth:

```text
many waits.
each wait has name.
set has cap.
first wins only if policy says first wins.
losers cancel only if policy says cancel.
partial result is still result.
```

## Non-Goals

- No macro first form.
- No hidden retry.
- No hidden branch cancellation.
- No unbounded result collection.
- No anonymous branch outcomes.
- No first-form heterogeneous reply set. Calls in one group share `R`;
  wrap outcomes in a user enum when branches differ.
- No pipeline sugar. A stage state machine is still the truth for
  ordered pipelines.

## Shape

Two PRs max:

1. `CallGroup<K, R>` + first-success race + one specimen.
2. join-all/partial-deadline report + `RequestContext`/child integration
   if the first PR stays small.

If PR 1 already exposes a bad abstraction, stop and document the copied
manual pattern instead of forcing PR 2.

## Rock 0 — Read Current Shapes

Read before code:

- `tina::PendingCallSet`;
- `cancel_call`;
- `Deadline`;
- 081 `RequestContext` if landed;
- `specimen_cancellation_chain`;
- `specimen_pool_cancel_reclaim`;
- `specimen_two_stage_pipeline` only as a "do not hide stage truth"
  warning;
- system specimen plans for `system_cache_with_fill`,
  `system_job_queue`, `system_checkout_saga`, and `system_rpc_gateway`.

## Rock 1 — Name The Data Shape

Prefer a small state helper over a policy framework.

Candidate:

```rust
pub struct CallGroup<K, R> {
    pending: PendingCallSet<K, R>,
    outcomes: Vec<NamedOutcome<K, R>>,
}
```

Maybe split into `RaceGroup` and `JoinGroup` only if the single type
gets fuzzy.

Rules:

- API home starts in `tina` only if it is runtime-agnostic over
  `CallHandle` / `PendingCallSet`; runtime-specific call builders stay
  in `tina-runtime`;
- fixed capacity;
- key type is user-owned and visible in outcomes;
- stores `CallHandle<R>` for cancellation;
- records outcomes by key;
- outcome storage is capped by group capacity; no uncapped `Vec` growth;
- no background sweep;
- caller removes keys explicitly on continuations;
- capacity report if the underlying `PendingCallSet` exposes enough.

## Rock 2 — First-Success Race

Ship one race helper:

```text
start N calls
first successful reply wins
cancel remaining losers visibly
return named result
```

Required output shape:

```rust
RaceReport<K, R> {
    winner: Option<(K, R)>,
    failures: Vec<(K, CallOutcome<R>)>,
    cancelled: Vec<(K, CancelOutcome)>,
    timed_out: bool,
}
```

The exact fields can change, but the facts cannot disappear.

Rules:

- caller chooses which outcomes count as success with an explicit
  predicate/classifier;
- loser cancellation is explicit in returned effects/messages;
- late loser replies become trace facts;
- branch key is in every report row.

## Rock 3 — Join-All With Partial Report

Ship one join helper:

```text
start N calls
collect every terminal outcome until all done or deadline
return partial report
```

Required facts:

- replied;
- full;
- closed;
- timeout;
- cancelled;
- still pending at aggregate deadline.

Use `Deadline`, not fresh relative duration arithmetic at every branch.

## Rock 4 — RequestContext Integration

If 081 has landed, show the normal service shape:

```text
capture RequestContext
start race/join
store group in state
reply_to_request when policy resolves
```

If 081 is not landed, block this rock or use the raw
`DeferredReply` shape with a TODO pointing at 081. Do not invent a
second request-context type.

## Rock 5 — Child Work Integration

If 084 has landed, prove a child-work shape:

- spawn N children with `spawn_observed`;
- start N child calls;
- join/race their replies;
- stop/cancel losers or children according to policy;
- no trace spelunking to discover addresses.

If 084 is not landed, keep this as docs/finding only.

## Rock 6 — Specimens

Update one or two high-signal specimens, not all of them.

Best candidates:

- `specimen_cancellation_chain` — race/cancel losers;
- `specimen_dynamic_worker_pool` — join child work if 084 landed;
- `system_cache_with_fill` — single-flight waiters if systems work has
  started;
- `system_checkout_saga` — join/race branch work if systems work has
  started.

Do not update ordered pipelines unless the helper preserves named stage
truth. Long pipeline code is acceptable.

## Required Proof

- fill group to capacity, reject one more with typed `Full`;
- first-success race cancels losers and records each cancel outcome;
- late loser reply is rejected visibly in trace;
- join-all returns all replies when all finish;
- join-all returns partial report at deadline;
- duplicate key is typed error, not overwrite;
- fill -> cancel/complete -> refill works;
- simulator proof for at least one race or join scenario;
- no helper hides retry or idempotency policy.

## Done Means

- Tina has a copied pattern for `select!`/`join!`-shaped workflows that
  keeps branch names and pressure visible;
- users no longer hand-roll the same bounded map/report/cancel loop in
  every race/join workflow;
- docs say when not to use it: ordered pipelines and simple single calls
  stay as ordinary message variants;
- at least one real specimen gets smaller without losing semantic truth.
