# 085 Race / Join Helpers

## Status

- Phase 085 start note:
  - chosen API home: `tina-runtime`, because first-success reports speak
    `CallOutcome` plus visible `cancel_call` outcomes.
  - chosen bounded storage shape: fixed-cap `Vec` entries with a
    per-branch generation token; no growing `HashMap`.
  - chosen specimen: `specimen_cancellation_chain`, because it already
    exposes cancel truth without hiding pipeline stages.
- Done: bounded named `CallGroup`, generation-token ABA guard,
  first-success race report vocabulary, explicit loser cancel requests,
  cancel-outcome-complete reports, RequestContext proof, owner-stop
  drain proof, `specimen_cancellation_chain` update, tests, clippy.
- Open: none for first-success first form.
- Deferred: join-all helper, macros, fake `select!`, hidden retry,
  stream select, policy framework, heterogeneous reply groups,
  child-ref sugar.

## Goal

Tina needs honest `select!` / `join!` / `JoinSet` shapes.

Not syntax cosplay. Keep names, caps, deadlines, cancels, and partial
results visible.

Grug truth:

```text
many waits.
each wait has name.
set has cap.
winner policy explicit.
loser cancel explicit.
partial is still truth.
```

## Non-Goals

- No macro first form.
- No hidden retry.
- No hidden loser cancel.
- No unbounded result Vec.
- No anonymous branch outcome.
- No heterogeneous reply set; use a user enum.
- No pipeline sugar.

## Shape

One PR.

Ship `CallGroup<K, R>` + first-success race + one specimen. Add
join-all / partial-deadline report and `RequestContext` / child-ref
integration in the same PR only if still boring. If it smells wrong,
stop and document the manual pattern.

## Rock 0 — Read First

- `PendingCallSet`;
- `cancel_call`;
- `Deadline`;
- 086 `CallContext` / reply obligation if landed; otherwise use the
  current `RequestContext` shape and migrate after 086;
- `specimen_cancellation_chain`;
- `specimen_pool_cancel_reclaim`;
- `specimen_two_stage_pipeline` as warning: do not hide stages.

## Rock 1 — CallGroup

Prefer one small state helper:

```rust
pub struct CallGroup<K, R> {
    pending: PendingCallSet<K, R>,
    outcomes: capped storage,
}
```

Rules:

- fixed capacity;
- one reply type `R`;
- key names every branch;
- stores `CallHandle<R>`;
- outcome storage capped by group capacity;
- duplicate key is typed error;
- no background sweep;
- user removes on continuations.

API home starts in `tina` only if runtime-agnostic. Runtime call-builder
sugar stays in `tina-runtime`.

## Rock 2 — First-Success Race

Ship one race:

```text
start N calls
first successful reply wins
cancel losers
wait for loser cancel outcomes
return named report
```

Report must include:

- winner key + reply;
- failed branch outcomes;
- loser cancel outcomes;
- timeout / no winner.

Rules:

- caller supplies success classifier;
- losers cancelled visibly;
- report waits for cancel outcomes;
- late loser replies are trace facts;
- every row has branch key.

No "reply winner now, cancel later" first form.

## Rock 3 — Join-All

Ship only if the first-success shape stays simple.

Join:

```text
start N calls
collect terminal outcomes
deadline may produce partial report
```

Report facts:

- replied;
- full;
- closed;
- timeout;
- cancelled;
- still pending at aggregate deadline.

Use `Deadline`.

## Rock 4 — CallContext / RequestContext

If 086 landed, prefer:

```text
take CallContext
convert to RequestContext if reply is later
start race/join
store group
reply_to_request once
owner stop cancels/drains group
```

If 086 has not landed, use the current `RequestContext` shape. Do not
invent a second request context.

## Rock 5 — Child Work

If 084 landed:

- spawn N children with `spawn_observed`;
- call children;
- race/join replies;
- cancel/stop losers by policy;
- no trace spelunking.

If 084 has not landed, leave this as docs/finding.

## Specimens

Pick one or two:

- `specimen_cancellation_chain`;
- `specimen_dynamic_worker_pool` if 084 landed;
- `system_cache_with_fill` if systems work started;
- `system_checkout_saga` if systems work started.

Do not update ordered pipelines unless stage truth stays visible.

## Proof

Required:

- fill group, next insert returns `Full`;
- duplicate key is typed error;
- first-success cancels losers;
- report waits for cancel outcomes;
- late loser reply is traced/rejected;
- fill -> cancel/complete -> refill works;
- no hidden retry/idempotency.

If join-all ships:

- join-all returns all replies;
- deadline returns partial report;
- sim proves one race/join scenario;
- `RequestContext` replies once and drains on owner stop.

## Done

- copied pattern for race/join workflows exists;
- branch names and pressure stay visible;
- at least one specimen gets smaller;
- docs say when not to use it.
