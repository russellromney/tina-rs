# Reviews and Decisions: 003 Mariner Stop And Abandon

## Round 1: Spec Diff Review

Artifact reviewed:

- `.intent/phases/003-mariner-stop-and-abandon/spec-diff.md`

Read against:

- `.intent/SYSTEM.md`
- `.intent/phases/002-mariner-local-send-dispatch/spec-diff.md`
- `.intent/phases/002-mariner-local-send-dispatch/reviews.md` (the slice
  002 review that flagged this gap)
- `tina-runtime-current/src/lib.rs` (current `Effect::Stop` handling and
  mailbox close semantics)
- `tina-mailbox-spsc/src/lib.rs` (close vs recv contract)

### Positive conformance review

Judgment: passes

What is good about this spec diff:

- **P1.** It closes the exact buffered-message gap that the slice-002
  adversarial review flagged ("send accepted in this round, target stops
  in the same round can produce a message that is silently never handled").
  The runtime now claims that every accepted message has a visible ending
  in the trace.
- **P2.** It does not widen `Effect::Stop`. Stop stays immediate. The
  handler that returned `Stop` is still the last handler the isolate
  runs. The slice resists the easy mistake of turning Stop into
  "stop-after-drain."
- **P3.** The new `MessageAbandoned` event lives in the runtime trace
  vocabulary and explicitly carries runtime facts only, no user message
  payloads. That preserves the slice-001 commitment that the trace is
  the meaning model for replay and simulation, and matches the
  existing rule that trace events do not carry user payloads.
- **P4.** The "What does not change" section is broad and disciplined:
  no supervisor mechanism, no parent notification, no restart semantics,
  no Tokio poll loop, no cross-shard routing, no mailbox semantics
  changes. The slice intentionally refuses to grow into those.
- **P5.** Verification criteria are testable as written: `N`
  `MessageAbandoned` events for `N` buffered messages, FIFO order,
  causal link to `IsolateStopped`, empty mailbox after, repeated runs
  produce equal traces, sends after stop still produce
  `SendRejected { Closed }`. The criteria do not appeal to internals.

### Negative conformance review

Judgment: passes with three things to pin

These are gaps where the spec diff leaves room for a conformant
implementation that the spec author probably did not intend.

- **N1.** "Drop drained buffered messages without delivering them to the
  stopped isolate's handler" pins the handler-not-invoked half but
  leaves the value-disposition half implicit. In Rust the natural
  reading is "let `Drop` run on the message value as the runtime pops
  it from the mailbox." That is fine, but the spec diff should say so.
  Otherwise a future reader whose message type has a side-effecting
  `Drop` impl has to derive the rule.
- **N2.** Timing of abandonment within a step round is not pinned by
  the spec diff. The two natural choices are:
  1. immediately after `IsolateStopped`, before any subsequent
     isolate's handler runs in the same round, or
  2. deferred to the end of the round after all handlers run.
  The plan implies (1). The spec diff should pin (1) explicitly,
  because the trace order is observable and "(1) vs (2)" is a real
  semantic difference for a downstream simulator that walks the trace.
- **N3.** "`MessageAbandoned` events carry runtime facts only, not user
  message payloads" is a constraint on what the event must NOT carry.
  It does not enumerate what the event MUST carry. For replay tools to
  correlate an earlier `SendAccepted` with its eventual
  `MessageAbandoned`, the event needs at minimum the target isolate id.
  The spec diff should at least say "the event identifies which
  isolate's mailbox the message was abandoned from."

### Adversarial review

Judgment: passes, with two things worth being honest about

I tried to break the main claim of the slice ("every accepted message
has a visible ending in the runtime model").

- **A1.** "Visible ending" is true at the trace event level but not at
  the causal-link level. `MessageAbandoned` is caused by
  `IsolateStopped`. The original `SendAccepted` that buffered the
  message is not a second cause. So replay tooling that wants to walk
  from "this send was accepted" to "this send was abandoned" has to
  walk through trace state, not along a causal edge. That is the right
  call for now, because tina's trace is single-cause (slice 001/002
  invariant). It just means the slice's claim is "every accepted
  message has a visible end-event," not "every accepted message has a
  causal chain from accept to end." The spec diff should weaken or
  qualify that line so a future supervisor reviewer does not over-read
  it.
- **A2.** Abandonment is currently a trace fact, not a sender-visible
  outcome. A future supervisor could be tempted to promote
  `MessageAbandoned` into a sender callback or a parent notification.
  This slice is intentionally pre-supervision, so that promotion is out
  of scope, but the spec diff should explicitly say "abandonment is a
  runtime trace fact only; senders are not notified" so the next slice
  has to re-open intent before turning it into a sender-side outcome.

What I checked and found defended:

- Stop-then-self-send in one handler call: impossible. Handlers return
  one effect.
- Send-then-stop in the same step round across two isolates (B sends to
  A in this round, then A's handler returns Stop): the new send
  enqueues into A's mailbox, then A's drain emits `MessageAbandoned`.
  Trace shows `SendAccepted` earlier and `MessageAbandoned` later, with
  the abandonment causally linked to `IsolateStopped` rather than to
  the send. That is consistent with the slice's claim, modulo A1.
- Replay determinism for `MessageAbandoned`: events get IDs from the
  same counter as every other runtime event. FIFO drain order plus
  sequential pushes give deterministic IDs across reruns by
  construction.
- Sends after stop: still rejected with `SendRejected { Closed }`. The
  slice does not regress this.
- `Reply`, `Spawn`, `RestartChildren`-induced abandonment: none.
  Restart is not real yet, so there is no "replaced children" path to
  abandon. The slice correctly does not invent one.

---

## Round 2: Plan Review

Artifact reviewed:

- `.intent/phases/003-mariner-stop-and-abandon/plan.md`

Read against:

- the slice 003 spec diff and the Round 1 review above
- `tina-runtime-current/src/lib.rs:475-484` (current `ErasedEffect::Stop`
  arm in `step()`)
- `tina-mailbox-spsc/src/lib.rs:182-205` (recv after close)

### Positive conformance review

Judgment: passes

- **P1.** The Decisions section pins exactly the questions a spec-diff
  reviewer would raise: Stop stays immediate, the handler that returned
  Stop is the last one, FIFO drain order, single causal link to
  `IsolateStopped`, runtime-facts-only events, drained messages dropped
  not delivered, sends-after-stop unchanged. Each decision is a flat
  statement, not prose.
- **P2.** Scope is small and numbered. Five items. No "and also" line
  buried in a paragraph. Stepping rules and send dispatch rules from
  slice 002 are explicitly held constant.
- **P3.** Traps are sharp and forbid the right things: no deferred
  Stop, no handler runs for abandoned messages, no supervisor or
  parent-notification behavior, no payloads in trace events, no
  cross-shard, no backpressure rule changes. They line up with the
  spec diff's "What does not change" almost line for line.
- **P4.** Acceptance section maps the spec diff's verification criteria
  to acceptance conditions one-for-one. No silent additions, no silent
  omissions.
- **P5.** The plan is honest about reusing slice 002's existing
  `mailbox.close()` on Stop. The new work is the drain loop plus the
  `MessageAbandoned` event. The plan does not pretend this is a larger
  rewrite than it is.

### Negative conformance review

Judgment: passes with four things to tighten

- **N1.** Decisions say "after the runtime records `IsolateStopped`, it
  drains any already-buffered messages from that isolate's mailbox
  immediately." "Immediately" is doing real work here. Pin it: "before
  any subsequent isolate's handler runs in the same step round." That
  closes Round-1 finding **N2**.
- **N2.** The plan trusts the SPSC mailbox contract that
  `mailbox.close()` followed by `mailbox.recv()` returns buffered
  values in FIFO order rather than dropping them. That contract holds
  today (`tina-mailbox-spsc/src/lib.rs`: `recv()` does not gate on
  closed state). The plan should name that contract dependency
  explicitly so a future mailbox swap that drops buffered values on
  close cannot silently break this slice.
- **N3.** References cite
  `.intent/phases/002-mariner-local-send-dispatch/reviews.md`. That
  filename is the historical artifact for slice 002. The IDD canonical
  name going forward is `reviews_and_decisions.md`. Slice 003's own
  review file should adopt the canonical name (this file) and the
  plan's references list should be updated when convenient. Not a
  blocker, just stop perpetuating the old name in new slices.
- **N4.** Scope step 2 lists "marks the isolate stopped, closes the
  isolate mailbox, records `IsolateStopped`, drains all still-buffered
  messages, records one `MessageAbandoned` event per drained message."
  The first three are already done by slice 002. The plan should say
  "the slice adds the last two steps; the first three are preserved
  from slice 002" so the actual delta is visible to a reviewer who
  diffs the runtime.

### Adversarial review

Judgment: passes, with three things worth pinning before implementation

- **A1.** Plan picks single causal link: `MessageAbandoned` caused by
  `IsolateStopped`. The originating `SendAccepted` is not a second
  cause. That is consistent with the rest of the trace, which is
  single-`caused_by`. It is the right call. Pin it explicitly so a
  future "let's add a second cause for replay" change has to reopen
  intent: "`MessageAbandoned` has exactly one causal link, to the
  `IsolateStopped` event of the same isolate; the originating
  `SendAccepted` is recoverable through trace walking, not through a
  second causal edge."
- **A2.** Order across multiple isolates stopping in the same step
  round is not pinned. Two isolates can both return Stop in one round
  because each gets one snapshot per round. The implementation will
  emit, in registration order: A's `IsolateStopped`, A's
  `MessageAbandoned` events, B's `IsolateStopped`, B's
  `MessageAbandoned` events. That is deterministic and matches slice
  002's registration-order invariant. It is worth one sentence in
  Decisions: "If two isolates stop in the same step round, their
  abandonment events are interleaved by registration order: each
  isolate's drain finishes before the next isolate's handler runs."
- **A3.** No per-step bound on the number of `MessageAbandoned` events
  emitted in one round. A 1000-message buffered mailbox emits 1000
  abandonment events inside one step. That is fine for replay
  determinism and acceptable here, but the plan should at least name
  it: "the drain loop is unbounded in event count for one step; the
  bound comes from the mailbox capacity, not from the runtime."
  Otherwise a future reader looking at unexpectedly large trace bursts
  will wonder if it is a defect.

What I checked and found defended:

- The drain happens inside the existing `ErasedEffect::Stop` arm of
  `step()`, which already does `stopped.set(true)` and
  `mailbox.close()`. Adding a `while let Some(_) = mailbox.recv_boxed()`
  loop after `IsolateStopped` is a small, local change.
- `recv_boxed()` does not gate on closed state, so close-then-drain
  works as written.
- Stepping order, send dispatch rules, send-rejected-Closed semantics,
  and trace single-`caused_by` shape are all explicitly held constant.

---

## Decisions to feed back to Session A before implementation

The plan is implementable as written. Before implementation starts, the
following are worth pinning either in the spec diff or in the plan's
Decisions:

1. Drained messages are destroyed in place (their `Drop` runs); the
   isolate's handler is never invoked with an abandoned message.
   (Round 1 N1.)
2. Abandonment runs inside the same step round, immediately after
   `IsolateStopped`, before any subsequent isolate's handler runs in
   that round. (Round 1 N2 and Round 2 N1.)
3. `MessageAbandoned` carries enough runtime fact to identify which
   isolate the message was abandoned from, but no user payload.
   (Round 1 N3.)
4. Abandonment is a runtime trace fact only. Senders are not notified.
   Promoting it to a sender-visible outcome requires reopening intent.
   (Round 1 A2.)
5. `MessageAbandoned` has exactly one causal link, to the matching
   `IsolateStopped`. The originating `SendAccepted` is recoverable
   through trace walking, not through a second causal edge.
   (Round 2 A1.)
6. If two isolates stop in the same step round, abandonment events are
   interleaved by registration order: each isolate's drain finishes
   before the next isolate's handler runs.
   (Round 2 A2.)
7. The drain loop is unbounded in event count per step. The bound comes
   from the mailbox capacity, not from the runtime.
   (Round 2 A3.)
8. Slice 003 depends on the SPSC mailbox contract that `close()` does
   not drop buffered values and `recv()` continues to return them in
   FIFO order regardless of closed state.
   (Round 2 N2.)

None of these are blockers. They are decisions that should land
explicitly in the artifacts so the next reviewer (or the implementation
review) does not have to derive them.

## Session A Responses

Accepted all eight feed-back decisions.

How they land:

1. Drained messages are destroyed in place and never delivered to a
   handler.
   Decision: accept.
   Artifact change: spec diff and plan both say drained buffered
   messages are dropped in place and handlers are never invoked for
   them.

2. Abandonment runs immediately after `IsolateStopped`, before any later
   isolate handler in the same step round.
   Decision: accept.
   Artifact change: plan Decisions and Acceptance pin same-round drain
   timing explicitly.

3. `MessageAbandoned` identifies which isolate's mailbox the message was
   abandoned from, but carries no user payload.
   Decision: accept.
   Artifact change: spec diff and plan both pin the minimum runtime fact
   carried by the event.

4. Abandonment is a runtime trace fact only. Senders are not notified.
   Decision: accept.
   Artifact change: spec diff and plan both state that sender-visible
   outcomes are out of scope for this slice.

5. `MessageAbandoned` has exactly one causal link, to the matching
   `IsolateStopped`. The originating `SendAccepted` is recoverable only
   by trace walking.
   Decision: accept.
   Artifact change: plan Decisions pin the single-cause rule
   explicitly.

6. If two isolates stop in the same step round, abandonment events are
   interleaved by registration order and each isolate's drain finishes
   before the next isolate's handler runs.
   Decision: accept.
   Artifact change: plan Decisions pin this ordering explicitly.

7. The drain loop is unbounded in event count per step, bounded only by
   mailbox capacity.
   Decision: accept.
   Artifact change: plan Decisions and Acceptance call this out so large
   abandonment bursts are understood as intended behavior.

8. Slice 003 depends on the mailbox contract that `close()` does not
   discard buffered values and `recv()` continues to return them in FIFO
   order after close.
   Decision: accept.
   Artifact change: plan Decisions and References name this contract
   dependency explicitly.

---

## Round 3: Implementation Review

Artifacts reviewed:

- `tina-runtime-current/src/lib.rs` (Stop arm + new event variant)
- `tina-runtime-current/tests/local_send_dispatch.rs` (5 new tests)
- `make verify` (passes: 11 integration + 5 trace-core + 4 loom + clippy
  + docs)

Read against:

- `.intent/phases/003-mariner-stop-and-abandon/spec-diff.md`
- `.intent/phases/003-mariner-stop-and-abandon/plan.md`
- the eight Decisions accepted in Session A Responses above

### Positive conformance review

Judgment: passes

- **P1.** The implementation is small and local: one new
  `RuntimeEventKind::MessageAbandoned` variant, and a drain loop tacked
  onto the existing `ErasedEffect::Stop` arm in `step()`. Total runtime
  delta is 17 lines. Matches the plan's "small, local change"
  expectation exactly.
- **P2.** Every accepted Decision has a test that pins it concretely:
  - Decision 1 (drained messages dropped in place) → `DropTracker`
    counts `Drop` calls and the test asserts `drops.get() == 2`. The
    runtime never invokes the handler with abandoned payloads.
  - Decision 2 (drain runs same round, before later handlers) →
    `abandonment_happens_before_later_isolate_handlers_in_the_same_round`
    pins index ordering: `IsolateStopped < MessageAbandoned <
    later-isolate's MailboxAccepted`.
  - Decision 3 (event identifies the isolate) → every
    `MessageAbandoned` in the trace assertions carries
    `address.isolate()`, satisfied via the existing per-event
    `isolate_id` field rather than a new payload field.
  - Decision 4 (runtime trace fact only, senders not notified) → no
    new sender-side surface added; the trace is the only observable.
  - Decision 5 (single causal link to `IsolateStopped`) →
    whole-trace assertions show every `MessageAbandoned`'s `caused_by`
    points to the matching `IsolateStopped`, never to a `SendAccepted`.
  - Decision 6 (interleave-by-registration-order across multiple
    stops) → not directly tested. See N2.
  - Decision 7 (drain unbounded per step) → implicit; the loop has no
    cap, the tests pass with multiple buffered messages.
  - Decision 8 (mailbox close-then-recv contract) → relied on by the
    `while ... recv_boxed().is_some()` loop; the loom tests for
    `tina-mailbox-spsc` already cover the production mailbox.
- **P3.** `accepted_send_can_become_message_abandoned_when_target_stops_in_the_same_round`
  is the most valuable test in the slice. It pins the exact scenario
  the slice-002 review flagged: B sends to A this round, A stops this
  round, A's drain emits `MessageAbandoned`. Whole-trace equality, not
  spot checks.
- **P4.** `repeated_runs_with_abandonment_produce_identical_traces`
  preserves the slice-001/002 determinism property across the new
  event kind.
- **P5.** No widening: `tina` crate untouched, no public surface added
  beyond the `MessageAbandoned` variant, no Tokio, no supervisor, no
  cross-shard.

### Negative conformance review

Judgment: passes with two things to tighten

- **N1.** The new tests were appended to
  `tina-runtime-current/tests/local_send_dispatch.rs` rather than a new
  file like `tests/stop_and_abandon.rs`. The plan said only "Add
  integration tests in `tina-runtime-current/tests/`," so this is not a
  spec violation. But adding 310 lines of stop-and-abandon tests to a
  file named after slice 002 obscures the slice boundary. A future
  reader looking for slice-003 evidence has to grep, not just open the
  file. Recommend extracting to `tests/stop_and_abandon.rs`.
- **N2.** Decision 6 ("if two isolates stop in the same step round,
  abandonment events are interleaved by registration order, each
  drain finishes before the next isolate's handler runs") is not
  directly tested. The implementation gets it right by construction
  because each isolate's Stop arm runs in registration order inside
  `step()` and the drain happens inline. But the test set covers
  "stop + non-stop next isolate" and "send-then-target-stops," not
  "stop + stop-in-same-round." A regression where someone deferred the
  drain to end-of-round would not be caught. Recommend adding one
  test where two isolates both return `Stop` in the same round and
  the trace shows `A.IsolateStopped → A.MessageAbandoned* →
  B.IsolateStopped → B.MessageAbandoned*`.

### Adversarial review

Judgment: passes, with two things worth being honest about

- **A1.** The runtime-level tests use `TestMailbox`, not the real SPSC
  mailbox. So the close-then-drain dependency (Decision 8) is exercised
  end-to-end only through the loom tests in `tina-mailbox-spsc`, not
  through a runtime integration test. The two coverage paths together
  prove the property, but no single test exercises both crates at once.
  This matches slice 001/002's testing style and was previously
  accepted; flagging here only because the property is now load-bearing
  in two places (close-on-stop from 002, drain-after-close from 003).
- **A2.** Stop-with-empty-mailbox is implicitly covered (the `while
  ... .is_some()` loop simply does not execute), but has no dedicated
  test. The slice-001 stop test already exercises this path without
  abandonment. Not a defect; worth a one-line negative test for
  resilience: "stop with no buffered messages produces zero
  `MessageAbandoned` events and the trace ends at `IsolateStopped`."

What I checked and found defended:

- The drain loop drops the `Box<dyn Any>` returned by `recv_boxed()`
  at the end of each iteration. `DropTracker.drop()` increments the
  counter, and the test asserts the count matches the buffered-message
  count. Drained values genuinely run their `Drop` impl.
- `MessageAbandoned` events are causally linked to the matching
  `IsolateStopped` (single cause), pinned by whole-trace equality in
  two tests.
- `SendRejected { Closed }` semantics from slice 002 are preserved by
  the dedicated `sends_after_stop_still_become_closed` test.
- No new public types beyond the `MessageAbandoned` variant. Crate
  boundaries hold.
- `make verify` passes: 11 integration tests, 5 trace-core, 4 loom,
  clippy clean, docs build.

### Open Decisions

These are the things you (the human) need to weigh in on before this
slice is considered done:

1. **Test file split.** Move the 5 new tests from
   `tests/local_send_dispatch.rs` into their own
   `tests/stop_and_abandon.rs` for slice-boundary clarity? (Round 3 N1.)
   I recommend yes — it costs almost nothing and keeps slice evidence
   discoverable.

2. **Add a multi-stop ordering test.** Add one test where two isolates
   both return `Stop` in the same round, asserting registration-order
   interleaving of abandonment? (Round 3 N2.)
   I recommend yes — Decision 6 is otherwise only true by
   construction, with no regression catch.

3. **Add a stop-with-empty-mailbox test.** One line, asserts zero
   `MessageAbandoned` events when the mailbox holds only the Stop
   message? (Round 3 A2.)
   I recommend optional — it is a small belt-and-suspenders test.

If you take (1) and (2), the slice is solid. (3) is nice-to-have.

---

## Round 4: Final Implementation Review Pass

Artifacts reviewed:

- `tina-runtime-current/src/lib.rs` (unchanged since Round 3)
- `tina-runtime-current/tests/local_send_dispatch.rs` (slice-002 tests
  restored; small `Clone` impl change)
- `tina-runtime-current/tests/stop_and_abandon.rs` (new file, 7 tests)
- `make verify` (passes; clippy clean, docs build, loom green)

### Positive conformance review

Judgment: passes — slice is done

- **P1.** All three Round 3 open decisions are addressed:
  1. Test file split: the slice-003 tests are now in
     `tests/stop_and_abandon.rs`. `tests/local_send_dispatch.rs`
     is back to slice-002 scope (6 tests).
  2. Multi-stop ordering test: `two_stops_in_one_round_interleave_-
     abandonment_by_registration_order` asserts whole-trace equality
     for two isolates that both return `Stop` in one round. The
     trace shape is exactly:
     `first.IsolateStopped → first.MessageAbandoned →
     second.MailboxAccepted → ... → second.IsolateStopped →
     second.MessageAbandoned`. Decision 6 is now pinned by a test,
     not just by construction.
  3. Stop-with-empty-mailbox: `stop_with_empty_mailbox_produces_-
     no_message_abandoned_events` asserts the trace ends at
     `IsolateStopped` (event 4) with no `MessageAbandoned` events.
- **P2.** The slice-002 test file's only diff is a manual `Clone` impl
  for `TestMailbox<T>` instead of `#[derive(Clone)]`. That is a
  legitimate fix: `derive(Clone)` would force `T: Clone`, but
  slice-003 tests use `DropTracker` payloads that intentionally do not
  implement `Clone`. The manual impl clones the two `Rc<...>` fields
  without bounding `T`. Correct and minimal.
- **P3.** Test file is well-organized: shared isolate fixtures
  (`StopIsolate`, `StopAndAudit`, `StopSender`, `OrderIsolate`,
  `DropTracker`) are declared once at the top, then 7 named tests
  exercise distinct scenarios. No copy-paste boilerplate per test.
- **P4.** Final test inventory:
  - empty-mailbox stop produces zero abandonment events
  - non-empty-mailbox stop produces N abandonment events in FIFO order
  - drain happens before later isolates' handlers in the same round
  - two stops in one round interleave by registration order
  - cross-isolate send-then-stop yields `SendAccepted` →
    `MessageAbandoned`
  - sends after stop still produce `SendRejected { Closed }`
  - repeated runs produce identical traces
  Together these cover every Decision in the plan with a matching
  test, not just by construction.

### Negative conformance review

Judgment: passes, no drift

- Runtime delta is unchanged from Round 3: 17 lines in `lib.rs`.
- `tina` crate is untouched. No public surface added beyond the
  `MessageAbandoned` variant.
- No Tokio, supervisor, parent notification, restart, or cross-shard
  scope creep.
- `make verify` passes end-to-end including loom and clippy.

### Adversarial review

Judgment: passes, no remaining gaps worth blocking on

The remaining gap I noted in Round 3 A1 — that runtime tests use
`TestMailbox` rather than the real SPSC mailbox, so the close-then-drain
contract is exercised end-to-end only via loom — is a long-standing
project decision (slice 001/002 use the same convention). It is not
introduced by this slice. The loom tests for `tina-mailbox-spsc`
already prove `close_racing_with_recv_still_preserves_buffered_delivery`,
which is exactly the contract slice 003 depends on. Leaving it as-is.

### Final judgment

The slice is done. All eight pre-implementation Decisions land in the
artifacts. All three Round 3 open decisions are addressed. The
implementation is small, the tests are sharp, and `make verify` is
green.

Ready to commit and update `CHANGELOG.md` / `commits.txt` per the
SESSION_A end-of-phase loop.

## Session A Responses To Round 3

Accepted all three follow-ups.

How they land:

1. Split the stop-and-abandon tests into their own file.
   Decision: accept.
   Artifact change: slice-003 integration tests move out of
   `tina-runtime-current/tests/local_send_dispatch.rs` into
   `tina-runtime-current/tests/stop_and_abandon.rs` so slice-002 and
   slice-003 proof surfaces stay separate and easy to review.

2. Add a multi-stop ordering test.
   Decision: accept.
   Artifact change:
   `two_stops_in_one_round_interleave_abandonment_by_registration_order`
   pins Decision 6 directly instead of relying on construction alone.

3. Add a stop-with-empty-mailbox test.
   Decision: accept.
   Artifact change:
   `stop_with_empty_mailbox_produces_no_message_abandoned_events`
   pins the negative path explicitly so the trace shape does not drift.
