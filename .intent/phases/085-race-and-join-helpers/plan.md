# Phase 085: Race / Join Helpers

## Status

- Ready to implement.
- 084 child refs are landed.
- 086 call context reply obligation is landed.
- Not blocked by 082 capacity work.

## Grug Truth

Many waits.

Each wait has a name.

The group has a cap.

Winner policy is explicit.

Loser cancel is explicit.

Partial result is still truth.

No fake `select!`.

## Goal

Add Tina-shaped race/join helpers.

They should reduce repeated bookkeeping without hiding:

- branch names;
- pending capacity;
- deadlines;
- cancel outcomes;
- late replies;
- partial results;
- request reply authority.

One PR.

The helper is group state and report vocabulary. It is not a hidden runtime
task scheduler. The user still returns ordinary Tina effects.

## Non-Goals

- no macro first form;
- no hidden retry;
- no hidden loser cancel;
- no unbounded result vector;
- no anonymous branch outcome;
- no heterogeneous reply set; use a user enum;
- no pipeline sugar;
- no idempotency guessing.

## Rock 0: Read First

Read:

- `tina/src/pending_call_set.rs`;
- `tina/src/lib.rs` `CallContext` / `RequestContext`;
- `tina-runtime` cancel-call APIs;
- `tina-runtime` deadline APIs;
- `tina-runtime/tests/cancel_call.rs`;
- specimens:
  - `specimen_cancellation_chain`;
  - `specimen_pool_cancel_reclaim`;
  - `specimen_dynamic_worker_pool`;
  - `specimen_two_stage_pipeline` as a warning.

Write a tiny status note at the top of this file before coding:

- chosen API home;
- chosen bounded storage shape;
- chosen specimen.

## Rock 1: API Home

Prefer:

- pure helper/state types in `tina`;
- runtime effect/call-builder sugar in `tina-runtime`;
- sim proof in `tina-sim` if a sim-backed call path is touched.

Do not put runtime-owned cancel/deadline machinery in `tina`.

Do not invent a second pending-call table.

## Rock 2: CallGroup

Ship one small helper:

```rust
pub struct CallGroup<K, R> { ... }
```

Rules:

- state helper only; effects still stay visible at the handler;
- fixed capacity;
- bounded storage, no growing `HashMap`;
- one reply type `R`;
- every branch has key `K`;
- duplicate key is typed error;
- full group is typed error;
- stores call handle/token needed to cancel;
- stores terminal outcomes up to capacity;
- no background sweep on insert;
- old continuation cannot remove a newer entry for the same key.

Use generation/token pairing if key reuse is allowed.

If key reuse cannot be made safe, reject reuse until old continuation is
observed and removed.

## Rock 3: First-Success Race

Ship first-success race.

Shape:

```text
start N calls
each branch has key
caller supplies success classifier
first success wins
cancel losers
wait for cancel outcomes
return report
```

Report includes:

- winner key and reply;
- non-winning replies/failures already observed;
- loser cancel outcomes;
- aggregate timeout/no-winner;
- late loser reply trace fact if observed later.

Rules:

- success classifier is caller-owned;
- no automatic retry;
- no idempotency claim;
- losers are cancelled visibly;
- cancellation means Tina cancel truth, not magic external rollback;
- helper does not reply to caller until report is ready;
- every row in the report has a key.

No "reply winner now, cancel later" first form.

## Rock 4: Join-All

Ship join-all only if it stays small after Rock 3.

Shape:

```text
start N calls
collect terminal outcomes
deadline may return partial report
```

Report facts:

- replied;
- full;
- closed;
- rejected;
- timeout;
- cancelled;
- still pending at aggregate deadline.

Use `Deadline`.

If join-all starts growing a policy framework, stop and document the manual
pattern instead.

## Rock 5: Request Reply Authority

Race/join must work inside a call handler.

Blessed shape:

```text
handle_call gets CallContext
if reply is later, convert to RequestContext
store group with request context
when group finishes, reply_to_request once
owner stop cancels group and drains request context
```

Rules:

- no second request-context type;
- no side-channel oneshot;
- exactly one final reply/reject to caller;
- owner stop does not leak pending call slots;
- panic/stop cleanup uses existing RequestContext/deferred cleanup truth.

## Rock 6: Child Work

Use 084 only where it naturally helps.

Good:

- spawn N children with `spawn_observed`;
- store `ChildRef`s;
- call children;
- race/join replies;
- stop/cancel losers by explicit policy.

Bad:

- trace spelunking for child addresses;
- hidden child registry;
- helper that hides supervision policy.

## Rock 7: Specimen

Update one specimen.

Best targets:

- `specimen_cancellation_chain`;
- `specimen_dynamic_worker_pool`;
- one systems specimen if already present and small.

Do not update ordered pipelines unless stage names stay visible.

Specimen README must say when the helper is wrong:

- ordered pipeline;
- branch side effects are not safe to cancel;
- caller wants partial progress before loser cancellation finishes.

## Rock 8: Sim/DST Proof

Add one simulator proof if the helper can run over sim-backed calls.

Minimum:

- deterministic winner under seeded timing;
- loser cancellation visible;
- partial join deadline visible if join-all ships.

If sim proof needs fake APIs, stop and keep it live-runtime tested only.
Do not claim DST proof without sim coverage.

## Proof Targets

Required:

- fill group, next insert returns `Full`;
- duplicate key is typed error;
- key reuse cannot ABA-remove a newer entry;
- first success cancels losers;
- report waits for cancel outcomes;
- late loser reply is traced/rejected;
- fill -> cancel/complete -> refill works;
- RequestContext replies exactly once;
- owner stop drains/cancels group;
- no hidden retry/idempotency.

If join-all ships:

- all replies collected;
- deadline returns partial report;
- still-pending branches are named;
- refill after partial/cancel works.

## Stop Signs

Stop and report before broadening if:

- helper wants heterogeneous reply types;
- helper wants a macro;
- helper hides branch names;
- helper hides loser cancellation;
- helper needs unbounded storage;
- helper tries to make pipelines shorter by hiding stages;
- helper guesses whether work is safe to cancel.

## Landing Criteria

- One copied race pattern is smaller and still honest.
- Branch names and pressure stay visible.
- Loser cancellation is visible and reported.
- RequestContext integration is proved.
- Bounded storage and refill are proved.
- At least one specimen uses the helper.
- Docs say when not to use it.

## Hostile Review Notes

- Risk: fake `select!` sugar. Guardrail: named branches and explicit reports.
- Risk: hidden unbounded join set. Guardrail: fixed-cap storage and tests.
- Risk: cancel lies. Guardrail: report cancel outcomes and late replies.
- Risk: helper becomes scheduler. Guardrail: user still returns visible effects.
- Risk: key ABA bug. Guardrail: token/generation or reject key reuse.
- Risk: pipeline misuse. Guardrail: docs say pipelines stay explicit.
