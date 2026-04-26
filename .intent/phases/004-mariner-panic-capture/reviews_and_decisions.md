# Reviews and Decisions: 004 Mariner Panic Capture

## Round 1: Spec Diff Review

Artifact reviewed:

- `.intent/phases/004-mariner-panic-capture/spec-diff.md`

Read against:

- `.intent/SYSTEM.md`
- `.intent/phases/003-mariner-stop-and-abandon/spec-diff.md` and the
  matching `reviews_and_decisions.md`
- `tina-runtime-current/src/lib.rs` (current Stop/abandon path)
- `ROADMAP.md` (Phase Mariner done-when criteria)

### Positive conformance review

Judgment: passes

- **P1.** Right slice to pick. Codex's reasoning for skipping Reply
  and Spawn is honest: Reply has no caller/request model yet, Spawn
  has no runtime-owned mailbox factory yet. Panic capture is the
  smallest move that closes the only remaining real-world failure
  mode in `CurrentRuntime` and gets us closer to supervision (which
  Phase Mariner's done-when criteria explicitly require).
- **P2.** Reuses the slice-003 abandonment path instead of inventing a
  parallel mechanism. Panic-stops and explicit-stops walk the same
  drain code, so there is one stop-shape, not two.
- **P3.** Trace-shaped verification criteria. The expected event
  sequence (`MailboxAccepted → HandlerStarted → HandlerPanicked →
  IsolateStopped`) is concrete and testable, not prose.
- **P4.** "What does not change" is broad and disciplined: no
  supervision policy execution, no restart budgets, no parent
  notification, no panic payloads in the trace, no `Reply`/`Spawn`/
  `RestartChildren` execution. Slice resists the easy widening into
  supervision.
- **P5.** Same-round continuation after panic is explicit. That is the
  single most important property for "trace stays deterministic when
  one isolate fails," and the spec calls it out as both a What-Changes
  item and a verification criterion.

### Negative conformance review

Judgment: passes with three things to pin

- **N1.** The spec says "the runtime marks that isolate stopped,
  closes its mailbox, records `IsolateStopped`, and drains any
  already-buffered messages as `MessageAbandoned`," but it does not
  pin the causal edge for `IsolateStopped` in the panic case. Slice
  003 left that edge implied (post-`HandlerFinished{Stop}`). In slice
  004 the natural cause is `HandlerPanicked`. Pin it: "in the panic
  case, `IsolateStopped` is caused by the matching `HandlerPanicked`
  event."
- **N2.** Panic-mode dependency is not declared. `catch_unwind` only
  catches unwinding panics; if a downstream binary builds with
  `panic = "abort"`, this slice silently does nothing and the whole
  process dies. Either pin "tina-runtime-current requires the
  consuming binary to use `panic = "unwind"`" or document it as a
  known limitation. This is not a hypothetical — many production Rust
  binaries set `panic = "abort"` to shrink size.
- **N3.** What happens if a buffered message's `Drop` impl panics
  during the abandonment drain is not addressed. The drain loop runs
  *after* the per-handler catch boundary, so a `Drop` panic during
  abandonment escapes and aborts the round. Either declare
  Drop-panics-on-buffered-messages out of scope (acceptable, but
  worth saying) or wrap the drain in its own `catch_unwind`.

### Adversarial review

Judgment: passes, with one asymmetric risk worth being honest about

- **A1.** The slice claims "the rest of the shard behaves
  deterministically" after one isolate panics. That is true for the
  panicking isolate's normal drain path. It is *not* true if a
  buffered message's `Drop` impl itself panics — that panic escapes
  the catch boundary and aborts the round, defeating the slice's
  headline property in exactly the case the slice was meant to
  defend. This is the same risk surface as N3, viewed from the other
  side: the spec promises round-survival, but does not commit to the
  mechanism that enforces it on the abandonment path.
- **A2.** `IsolateStopped` now has two upstream cause-kinds:
  `HandlerFinished{Stop}` (slice 003) and `HandlerPanicked` (slice
  004). The event vocabulary stays single-cause per event, but a
  replay walker that assumes "IsolateStopped is always caused by
  HandlerFinished{Stop}" will break. The slice should explicitly
  acknowledge that downstream tools must handle both upstream causes.

What I checked and found defended:

- Same-round continuation: spec explicitly carries this property.
- Inheriting slice-003 send-then-target-stops semantics: a Send
  accepted into the panicking isolate's mailbox earlier in the same
  round will be drained as `MessageAbandoned`. The slice does not
  regress this.
- No new supervision concepts. No restart, no policy, no parent
  notification.
- No panic payloads in trace, preserving the runtime-facts-only rule
  for trace events.

---

## Round 2: Plan Review

Artifact reviewed:

- `.intent/phases/004-mariner-panic-capture/plan.md`

Read against:

- the slice 004 spec diff and Round 1 above
- `tina-runtime-current/src/lib.rs:434-498` (current step loop and
  Stop/abandon path)
- slice 002's `send_to_unknown_isolate_panics` test (the
  programmer-error panic that this slice's Decisions section
  explicitly preserves)

### Positive conformance review

Judgment: passes

- **P1.** Decisions name `catch_unwind` at the dispatcher boundary
  explicitly. This is the implementation choice that needs to be
  pinned in writing, not derived later.
- **P2.** "Reuse the existing stop-and-abandon path after panic so
  buffered messages are drained and traced consistently" is in scope.
  No fork in stop semantics.
- **P3.** Same-round continuation is in Decisions, not just in the
  spec diff. Acceptance line "A later isolate in the same round still
  runs after an earlier isolate panics" is testable.
- **P4.** Programmer-error panic for sends to unknown isolate IDs is
  preserved explicitly. That is the right call (slice 002 made it a
  fail-fast contract) and avoids the easy mistake of catching all
  panics including dispatcher bugs.
- **P5.** Traps are sharp: don't sneak in restart, don't carry panic
  payloads, don't let one panic abort the round, don't quietly turn
  panic into `EffectObserved`, don't invent a request/reply or
  spawn-execution model.

### Negative conformance review

Judgment: passes with four things to tighten

- **N1.** "`catch_unwind` at the dispatcher boundary" leaves the
  *scope* of the catch unspecified. The slice's own Decisions section
  also says "preserves the existing programmer-error panic for sends
  to unknown isolate IDs" — but if the catch wraps the entire match
  arm body (including the send dispatch that calls `index_of`), the
  unknown-target panic gets caught too, becomes a `HandlerPanicked`
  event for the *sender*, and the sender's isolate stops. Pin: "the
  catch wraps only the handler call (`handler.handle_boxed(...)`),
  not the surrounding dispatcher arms. Programmer-error panics from
  the dispatcher (unknown target, type-erasure mismatch) still
  propagate out of `step()`."
- **N2.** `UnwindSafe` / `AssertUnwindSafe` is not addressed. The
  handler call holds `&mut Shard` and isolate state through a
  `RefCell<Box<dyn ErasedHandler<S>>>`. `catch_unwind` requires the
  closure to be `UnwindSafe`, which neither of those satisfies. The
  right answer is `AssertUnwindSafe` at the call site, *not* adding
  `UnwindSafe` bounds to the public `Isolate` or `Shard` traits. Add
  a Trap: "do not add `UnwindSafe` bounds to the `Isolate` or
  `Shard` traits — wrap the call site in `AssertUnwindSafe`."
- **N3.** Drop-panic-during-drain is not addressed (Round 1 N3/A1
  from the spec-diff side, viewed from the plan side). The drain
  loop runs after the per-handler catch, so a `Drop` panic on an
  abandoned message escapes. Either add it to Out Of Scope ("a
  panic from a buffered message's `Drop` impl during abandonment is
  user error and aborts the round") or extend the catch to cover
  the drain.
- **N4.** References list does not include `tina-mailbox-spsc`. Slice
  003 made the close-then-recv contract load-bearing, and slice 004
  inherits that dependency through the abandonment path. The
  reference belongs in the list.

### Adversarial review

Judgment: passes, with two angles worth pinning

- **A1.** A panicking handler returns no `Effect`, so no
  `HandlerFinished` is recorded and no `Send`/`Reply`/`Spawn`
  observation happens. The trace shape after panic is therefore
  `HandlerStarted → HandlerPanicked → IsolateStopped → 0..N
  MessageAbandoned`. Plan implies this but does not pin "no
  `HandlerFinished` is emitted on the panic path." A future reader or
  a regression that emits a phantom `HandlerFinished` would not be
  caught by the current acceptance criteria. Worth one sentence in
  Decisions.
- **A2.** `IsolateStopped` now has two upstream cause-kinds depending
  on path. Plan does not pin "`IsolateStopped` is caused by
  `HandlerFinished{Stop}` on the explicit-stop path and by
  `HandlerPanicked` on the panic path; in both cases there is exactly
  one causal edge." This matches the slice-002/003 single-cause
  invariant; just say so.

What I checked and found defended:

- Reuses the existing `ErasedEffect::Stop` arm's drain code via the
  shared mailbox/event infrastructure.
- Single-cause trace shape preserved.
- No new public types beyond `HandlerPanicked` (and the runtime
  internals to support catching the panic).
- No `tina` crate changes implied; runtime concern stays in the
  runtime crate.

---

## Open Decisions

Five things you (the human) need to weigh in on before this slice is
implementable as written:

1. **Pin the `catch_unwind` scope.** It must wrap only the handler
   call, not the surrounding dispatcher arms. Otherwise the
   programmer-error panic for unknown target IDs (which the plan
   explicitly says to preserve) gets caught too. (Round 2 N1.)
2. **Pin `AssertUnwindSafe` at the call site.** Do *not* add
   `UnwindSafe` bounds to the `Isolate` or `Shard` traits. The
   alternative is a viral trait bound that pollutes the public
   surface. (Round 2 N2.)
3. **Decide what happens if a buffered message's `Drop` impl panics
   during abandonment.** Either declare it user error and out of
   scope (acceptable, simpler), or wrap the drain in its own
   `catch_unwind` (defensive, more code). I recommend the first; the
   round-survival promise is for handler panics, not for `Drop`
   panics. (Round 1 N3, Round 1 A1, Round 2 N3.)
4. **Pin the cause edge for `IsolateStopped` in the panic case.** It
   should be caused by the matching `HandlerPanicked`. Single causal
   link, just like `HandlerFinished{Stop} → IsolateStopped` on the
   explicit-stop path. (Round 1 N1, Round 2 A2.)
5. **Declare the panic-mode dependency.** `catch_unwind` only catches
   unwinding panics. If the consuming binary uses `panic = "abort"`,
   this slice silently does nothing. Either commit to "tina-runtime-
   current requires `panic = "unwind"`" or document it as a known
   limitation. (Round 1 N2.)

If you take all five, the slice is ready to implement. None are
blockers in the sense of "the code can't work without them" — but all
five are decisions that need to land in the artifacts before
implementation, not be derived later.

---

## Round 3: Implementation Review

Artifacts reviewed:

- `tina-runtime-current/src/lib.rs` (42-line delta)
- `tina-runtime-current/tests/panic_capture.rs` (new, 6 tests)
- `make verify` (passes; clippy clean, docs build, loom green)

### Positive conformance review

Judgment: passes — slice is done

- **P1.** All five pinned Decisions land exactly as written:
  1. `catch_unwind` wraps *only* `handler.handle_boxed(...)`. The
     surrounding `let mut handler = ...; catch_unwind(...)` block is
     scoped tightly; the send-dispatch arms remain outside it.
  2. `AssertUnwindSafe` is used at the call site. No `UnwindSafe`
     bounds were added to `Isolate`, `Shard`, or runtime
     registration APIs.
  3. Drop-panic-during-drain is unprotected: `stop_entry` calls
     `mailbox.recv_boxed()` and lets the returned `Box<dyn Any>`
     drop without a wrapping `catch_unwind`. Matches the documented
     out-of-scope policy.
  4. `IsolateStopped` in the panic path is caused by
     `HandlerPanicked`. The `stop_entry(index, isolate_id,
     handler_panicked.into())` call passes the panic event as the
     cause, and the test traces show event 4 (`IsolateStopped`)
     causally linked to event 3 (`HandlerPanicked`).
  5. `panic = "abort"` is documented in the module docstring as out
     of scope for this crate.
- **P2.** The `stop_entry` extraction is a quietly excellent
  secondary refactor. Both the explicit-Stop arm (slice 003) and the
  panic path (slice 004) now go through one drain function, so there
  is exactly one place that handles "mark stopped → close mailbox →
  record `IsolateStopped` → drain → emit `MessageAbandoned`*". Reduces
  the chance of a future slice adding a third stop path that diverges.
- **P3.** The `unknown_target_send_still_panics_instead_of_becoming_-
  handler_panicked` test is the right shape: it wraps
  `runtime.step()` in `catch_unwind` and asserts both that the
  result is `Err` *and* that the trace contains zero
  `HandlerPanicked` events while still containing the
  `HandlerFinished{Send}` and `SendDispatchAttempted` that preceded
  the panic. That positively pins the catch-boundary scoping
  decision (Open Decision 1).
- **P4.** The panic + abandonment test uses `DropTracker` payloads
  with FIFO assertion (`*dropped.borrow() == vec![1, 2]`), so drained
  messages are proven to (a) actually run their `Drop` and (b) come
  out in mailbox order.
- **P5.** Determinism is pinned by
  `repeated_runs_with_panic_events_produce_identical_traces`, which
  proves whole-trace equality across two fresh runs that include a
  panic.

### Negative conformance review

Judgment: passes with one thing worth adding

- **N1.** No test exercises "two isolates both panic in the same
  round." Slice 003 has the analogous test for two-Stops-in-one-round
  (`two_stops_in_one_round_interleave_abandonment_by_registration_-
  order`), which we explicitly asked for in that slice's
  implementation review. The registration-order interleaving property
  now flows through the shared `stop_entry` helper, so it holds for
  the panic path by construction — but a regression where someone
  changes one path's call shape and not the other would not be
  caught. Recommend adding `two_panics_in_one_round_interleave_-
  abandonment_by_registration_order` in the same shape as the slice-
  003 test.

### Adversarial review

Judgment: passes, with one subtlety worth being honest about

- **A1.** Soundness of `borrow_mut()` across panic. The
  implementation does:
  ```
  let effect = {
      let mut handler = self.entries[index].handler.borrow_mut();
      catch_unwind(AssertUnwindSafe(|| {
          handler.handle_boxed(message, &mut self.shard, isolate_id)
      }))
  };
  ```
  If the handler panics, the stack unwinds through the closure, the
  `RefMut` `Drop` releases the borrow, then `catch_unwind` returns
  `Err`. By the time `stop_entry` runs and re-accesses
  `self.entries[index]`, the borrow is gone. This is sound because
  `RefCell::borrow_mut()`'s `Drop` is panic-safe in std. It is worth
  knowing because this is the *only* place in the runtime where a
  `RefCell` borrow exists across a panic boundary, and the soundness
  story rests on `RefMut::Drop` doing the right thing during unwind.
  Not a defect; just a quiet load-bearing assumption.
- **A2.** The panic payload (`Box<dyn Any + Send>`) is dropped via
  the `Err(_)` arm. If the payload's own `Drop` panics, that escapes
  the catch (we are no longer inside `catch_unwind`). This is the
  same out-of-scope policy as Drop-panic-on-buffered-message — just a
  slightly different surface. Acceptable; matches the documented
  rule.

What I checked and found defended:

- `catch_unwind` scope: only the handler call. Send-dispatch panics
  (unknown target) propagate out, proven by the explicit test.
- No `UnwindSafe` bounds added to public traits. Confirmed by
  grepping the crate.
- `IsolateStopped` causally linked to `HandlerPanicked` in the panic
  path. Confirmed by whole-trace assertions in two tests.
- Buffered messages drained in FIFO order via the shared `stop_entry`
  helper. Confirmed by `DropTracker` ordering assertion.
- Same-round continuation: `later_isolates_still_run_after_panic_in_-
  same_round` proves the next isolate's handler runs after the
  panic-and-drain sequence completes.
- Sends after panic: `sends_after_panic_still_become_closed`
  preserves slice-002 `SendRejected { Closed }` semantics.
- Determinism preserved across the new event variant.
- `make verify` passes: 6 panic_capture + 7 stop_and_abandon + 6
  local_send_dispatch + 5 trace_core + 4 loom + clippy + docs.

### Open Decisions

One thing for you (the human) to weigh in on:

1. **Add a "two panics in one round" test.** Mirrors the slice-003
   `two_stops_in_one_round_interleave_abandonment_by_registration_-
   order` test for the panic path. The property holds by construction
   through the shared `stop_entry` helper, but the test would catch a
   regression where someone changes one path's call shape and not the
   other. — Recommend yes; it is a one-test addition that closes the
   parity gap with slice 003.

If you take it, slice 004 is fully done. If not, the slice still
passes — the property is proven by construction via `stop_entry` and
the slice-003 test that exercises the helper for the explicit-Stop
path.
