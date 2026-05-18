# Hostile Review — Phase 105 Request-Scoped Cancellation

## What landed

- `tina-runtime::scope` module: `RequestScopeId`, `RequestScope`,
  `RequestScopeSet`, `ScopedCallHandle`, `ScopeCancelCause`,
  `ScopeCancelReport`, `ScopeChildReport`, and the
  `DeferredScopedCall` admission helper with `CallContextScopeExt`
  blessed patterns:
    - `call_ctx.defer_scoped(&scope, label, work).try_admit(pending, key, ...)` —
      atomic pending-set + scope admission with rollback on partial
      failure.
    - `call_ctx.defer_scoped(&scope, label, work).reply(key, ...)` —
      scope-only admission, no `PendingCancelableCallSet` involvement.
- 11 deterministic proofs in `tina-sim/tests/request_scope.rs`:
  cancel-before-delivery, cancel-after-deferred-capture,
  fill-cancel-refill, multi-rail single cancel, owner-stop registration
  block, `cancel_into_effect` Noop-on-empty, cancel-after-delivery,
  settled-rail report state, late worker reply after cancel surfaces as
  typed trace fact, owner-stop drain of a `RequestScopeSet`, cross-shard
  rail surfaces `CancelOutcome::WrongShard`.
- 5 deterministic negative-path proofs in
  `tina-sim/tests/request_scope_admit_failures.rs`: every
  `ScopedAdmitError` variant (scope-cancelled, scope-full, pending-full,
  pending-duplicate, **and the rollback path where pending succeeds and
  scope register then fails**) returns the token with caller authority
  recoverable, and the scope is left in a clean state.
- 1 live-runtime proof in `tina-runtime/tests/request_scope.rs`
  (two rails, one scope cancel, trace asserts both `CallCancelled`
  facts).
- New specimen `examples/specimen_request_scope_fanout/` with a smoke
  test that runs the full pattern against the threaded runtime: one
  driver, FANOUT rails registered into one scope, one cancel closes all
  pending rails, every late worker reply surfaces as a typed
  `CallReplyRejected { CallerCancelled }` / `DeferredReplyRejected`
  trace fact.
- Doc updates: `04-request-reply.md` (blessed pattern + section link),
  `14-lifecycle-and-shutdown.md` (rail truth table + bridge honesty
  pointer), scope module preamble (cross-shard semantics and
  `CallerCancelled` cause-name overlap).

## Findings resolved in this pass (post-review)

### CR-1 [P1] `try_admit` registered scope before pending insert — leak on pending failure

**Original symptom.** `try_admit` first registered the rail's shared
cell into the scope, then attempted to insert the token into the
pending-set. If the pending insert failed (`Full` / `DuplicateKey`),
the rejected token was returned correctly but the scope still held the
shared cell. Across N failed admissions, the scope's child cap would
silently fill with dead registrations until the scope was dropped or
cancelled.

**Fix.** Inverted the ordering. `try_admit` now:

1. Builds the pending token (caller authority captured).
2. Admits the token into the pending set first; on failure, the scope
   is untouched and the rejected token is returned.
3. Registers the rail into the scope second; on failure, the pending
   insert is **rolled back** via `pending.remove(&key, ticket)` and
   the recovered token is returned in the error.

Pinned with
`request_scope_admit_failures::try_admit_pending_full_returns_token_caller_sees_pending_full`
(asserts `scope.registered() == 1` after a failed admission) and
`request_scope_admit_failures::try_admit_rollback_on_scope_register_after_pending_succeed`
(forces the rollback path by pre-filling the scope to its cap; verifies
`scope.registered()` stays at 1 even though the pending insert
succeeded mid-call).

### CR-2 [P1] Missing `.reply` spelling from the plan example

**Original symptom.** Plan's `call_ctx.defer_scoped(scope, work).reply(key, Msg::Done)`
example had no implementation. Only `.try_admit(...)` existed, forcing
users into a `PendingCancelableCallSet` for every scope admission.

**Fix.** Added `DeferredScopedCall::reply(key, translator)` returning
`Result<(PendingCancelableCall, Effect), ScopedReplyError<K, Q, R>>`.
The user stores the pending token wherever they want (a single Option,
a small Vec, etc.); only scope registration is committed automatically.
`ScopedReplyError` mirrors `ScopedAdmitError` for the scope-only path
and has its own `into_token()` recovery helper.

### CR-3 [P1] `ScopedCallHandle` was dead surface

**Original symptom.** The type was declared but no blessed-path
function returned one.

**Fix.** Documented `scope_register(scope, label, handle)` as the
"keep typed handle + add scope coverage" entry point; clarified the
distinction from `RequestScope::register` (consumes handle, scope is
sole canceller) in module docs. The type is now reachable and its
purpose is clear.

### CR-4 [P2] No "cancel after delivery before reply" test

**Original symptom.** Plan listed this as a required proof; my test
suite only covered "cancel after deferred capture" (worker held the
slot).

**Fix.** Added `scope_cancel_after_delivery_before_reply_closes_wait`
which dispatches the call, drains so the worker handler has run, then
verifies `state_at_cancel == Pending` and that no reply reaches the
caller. Distinct trace coverage from the deferred-capture case.

### CR-5 [P2] No bridge-late-reply trace proof

**Original symptom.** Plan: "Cancel after bridge accepts work and
completes late." No DST proof.

**Fix.** Added `scope_late_worker_reply_after_cancel_is_typed_rejected_fact`.
After the scope cancels, the worker fires its late `reply_to`; the
runtime classifies it as `DeferredReplyRejected { CallerCancelled }`
exactly once. Bridge-style late completion behavior verified.

### CR-6 [P2] No owner-stop scope-set drain proof

**Original symptom.** Plan: "Owner stop cancels scopes and emits final
report." My test only checked that `register` was blocked after
synchronous cancel.

**Fix.** Added `scope_set_drain_on_owner_stop_cancels_every_scope`.
Three scopes admitted with mixed child counts; `set.drain()` visits all
of them; each is cancelled with `OwnerStopped`; the aggregate report
reports the correct child counts and the set is empty afterwards.

### CR-7 [P2] No cross-shard cancel proof through a scope

**Original symptom.** Plan: "Cross-shard caller/callee keeps cancel
cause." My implementation routed cross-shard rails through the existing
`CancelOutcome::WrongShard` rail but had no test pinning it.

**Fix.** Added `scope_cross_shard_rail_surfaces_wrong_shard_in_cancel_ack`.
A shared cell is stamped with a foreign shard id (99), registered into
a scope on the local shard, then cancelled. The cancel ack reaches the
service translator with `CancelOutcome::WrongShard`. The scope does
not silently swallow the failure.

### CR-8 [P2] Negative-path tests with token recovery missing

**Original symptom.** Every `ScopedAdmitError` variant carried a token
in the source, but no test asserted that a real
`defer_scoped(...).try_admit(...)` call produced that token and that
the caller could recover authority from it.

**Fix.** New file `tina-sim/tests/request_scope_admit_failures.rs`
with 5 end-to-end driver tests. Each test sends a `Submit` into a
driver isolate which exercises `try_admit`; the driver replies through
`token.into_request_context()`; a host isolate observes the reply.
Every error variant proven to return caller authority on a live path,
not just in source.

### CR-9 [P2] Plan example `.reply` shape exposed

Resolved as part of CR-2.

### CR-10 [P3] No "rail settled before cancel" report proof

**Original symptom.** `ScopeChildReport.state_at_cancel` could be
`Settled` per the API, but no test exercised that branch.

**Fix.** Added `scope_records_settled_rail_state_at_cancel`. Worker
replies before scope cancel; `cancel_synchronously` reports
`state_at_cancel == Settled` and `cancelled_count() == 0`.

### CR-16 [P3] `cancel_into_effects` returned `Vec<Effect>` ergonomically awkward

**Fix.** Added `RequestScope::cancel_into_effect` returning a single
`Effect<I>` — `Effect::Noop` on empty, the single effect on N=1, or
`Effect::Batch` on N>1. Asserted in
`scope_cancel_into_effect_returns_single_batched_effect`. The
multi-effect form is still available for callers that want per-rail
visibility.

## Findings still open (follow-on phases)

### Finding F1 [P1] Scope cancel covers only the `call_cancelable` rail

Sleep, raw TCP/TLS read/write, and body sources have no first-form
`CallHandle`. Scope cancellation cannot close them. Documented in
`14-lifecycle-and-shutdown.md`. Adding cancelable variants
(`sleep_cancelable`, `tcp_read_cancelable`, …) is the obvious next
slice; the scope is generic over `Arc<CallHandleShared>` and will
absorb them without further work.

### Finding F2 [P1] No HTTP/WebSocket/gRPC listener integration

Connection-close → scope cancel and server shutdown drain → scope
cancel are not yet wired. The scope is `Clone + Send`, so the listener
can hold a clone and call `cancel_into_effect(...)`, but the listener
state machine doesn't do that yet. Deferred to the same phase that
ships the body-source cancellation rail (F1) — doing them together
avoids landing two half-paths.

### Finding F3 [P2] Job queue specimen unchanged

The `system_job_queue` specimen has a one-rail-per-submit shape; the
scope vocabulary adds nothing on top of its existing
`PendingCancelableCallSet`. The multi-rail proof now has a real
specimen (`examples/specimen_request_scope_fanout/`) and DST coverage,
so the job queue stays in its current shape.

### Finding F4 [P2] `mini_saas_api` specimen unchanged

Same logic as F3 + waiting on F2 (HTTP listener disconnect plumbing).
Adopting it mechanically once F2 lands is straightforward.

### Finding F5 [P3] `system_media_ingest_pipeline` never existed

Named in the plan as a third user proof; no such directory exists.
Treated as a forward reference that lands alongside the body-source
cancellation rail (F1).

### Finding F6 [P3] Scope cancel uses rail-level `CancelCause::CallerCancelled`

When the scope issues `cancel_call_with_handle`, the runtime trace
records `CallCancelled { cause: CallerCancelled }` regardless of the
`ScopeCancelCause` the service chose. The scope-level cause is in the
synchronous report; it is not on the per-rail trace fact. Acceptable
for now — adding a `ScopeCancelled(ScopeCancelCause)` variant on
`tina::CancelCause` would mean threading the cause through every
cancel rail in the runtime, which is broader than one phase. The
synchronous report gives the service the cause; consumers needing it
in trace can log it separately.

## Verdict

The primitive is real, bounded, honest, and proven on two runtimes
(deterministic simulator + live threaded runtime) plus a user-facing
specimen smoke test. Every documented failure mode now has an
end-to-end driver test demonstrating token recovery. The one real bug
that survived the first pass — `try_admit` leaking scope registration
on pending failure — was caught in the second hostile review pass and
fixed with a load-bearing rollback path that itself has a dedicated
test (`try_admit_rollback_on_scope_register_after_pending_succeed`).

Open follow-on work (F1, F2, F4, F5) all expand the primitive's
*reach*, not its *shape*. None requires redesigning the API. The
phase's required-proof checklist is now complete except for the items
that genuinely depend on rails that don't yet exist (sleep/TCP/body
cancellation handles).
