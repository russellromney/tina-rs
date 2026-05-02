# 024 Mercury Plan Review

Status: superseded by Session B plan rewrite. The review remains as the
historical first review of the roadmap seed; the current `plan.md` now makes
the overload lab the first proof and moves monoio behind the practical Tokio
current-thread backend decision.

Reviewed:

- `ROADMAP.md` Mercury section as the current plan seed

Verdict: right next phase, not ready to implement from the roadmap entry alone.
Grug likes the target, but Mercury needs a real `plan.md` before build. The
roadmap now protects the ordering: Mercury before Gemini. Good. The missing
work is mostly pinning semantics and slice order so we do not accidentally
build three half-solutions to "tryable runtime."

## What Looks Right

- Mercury is correctly placed before Gemini. Docs/release can wait until the
  runtime contract is true.
- The phase target is honest: selected Tokio-shaped workloads, not broad Tokio
  replacement.
- The work list matches the Tina/Odin spirit: thread-per-shard runtime,
  bounded overload feedback, timeout request/reply, live supervision, and DST
  proof.
- The plan keeps monoio as a decision/spike rather than pretending current
  threaded+Betelgeuse is automatically production-shaped.

## Findings

1. **[P1] Roadmap seed is not an implementation plan.**
   Mercury currently exists as a roadmap row plus a short phase section. That
   is enough to preserve build order, but not enough to launch implementation.
   It needs `.intent/phases/024-mercury-production-shaped-runtime-contract/plan.md`
   with build steps, proof matrix, pause gates, and exact non-claims.

2. **[P1] User-visible backpressure semantics are not pinned.**
   This is the most load-bearing Mercury decision. Tina-Odin gets immediate
   `ctx_send` outcomes because send happens inside the handler. tina-rs handlers
   return effects, so a normal `send(...)` cannot synchronously return
   `Accepted` / `Full` / `Closed` to the same handler without changing the
   effect model. The plan must choose the semantic shape:
   - a send-attempt effect that later delivers a typed outcome message,
   - a context-side immediate `try_send` escape hatch,
   - or a narrower "observed send" call-like helper.

   Grug bias: preserve effect-returning handlers and make observed send a
   later-message outcome, then prove ordinary fire-and-forget `send(...)` stays
   simple.

3. **[P1] Isolate-to-isolate call depends on the backpressure decision.**
   A timeout call is likely "send request, wait for reply, timeout if no reply,"
   but request admission can itself be `Full` / `Closed`. The plan must pin how
   those outcomes compose: immediate call failure message, later call failure,
   trace-only failure, or panic. Mandatory timeout is good, but not sufficient.

4. **[P1] Monoio spike needs a narrow success/fallback bar.**
   "Spike monoio" can sprawl. The plan should define a smallest acceptable
   substrate proof: one shard, one current-thread monoio reactor, runtime-owned
   TCP echo, synchronous handlers unchanged, and oracle/substrate trace subset.
   If that fights the model, the phase should record a fallback decision and
   continue Mercury on threaded+Betelgeuse rather than stalling everything.

5. **[P1] One phase package needs explicit slice order.**
   The scope is right but large. Order matters:
   1. substrate seam audit + monoio decision spike
   2. observed send outcome
   3. timeout call
   4. runner lifecycle hardening
   5. live spawned-child cross-shard proof
   6. live supervision proof
   7. dogfood service
   8. capacity/allocation claim
   9. public-ish DST harness

   Dogfood should not start before backpressure and timeout semantics exist.

6. **[P2] Dogfood workload is not specified enough.**
   "Real service-shaped workload" is right, but closeout needs a named shape.
   Recommendation: TCP ingress with per-connection isolate, worker/session shard,
   timeout call to worker/session, overload policy on observed send/call
   failure, supervised worker restart, and simulator replay/checker over the
   same isolate logic.

7. **[P2] Capacity/allocation claim needs expected direction.**
   Mercury should probably not chase Tina-Odin's full no-hidden-allocation story
   yet. Pin expected direction as: bounded queues and bounded configured
   capacities are claimed; broad runtime allocation-free behavior is not
   claimed unless a focused probe proves a narrower path.

8. **[P2] Public-ish DST harness scope should be tiny.**
   Avoid turning Mercury into a docs/framework packaging phase. The harness can
   be one reusable test helper/API that runs a user workload under seed/replay
   and optional checker. It does not need a polished guide until Gemini.

## Suggested Done Means

- A real Mercury `plan.md` exists and resolves the semantic choices above.
- Monoio is either proven by a tiny substrate workload or explicitly deferred
  with reason.
- App code has one blessed way to observe send/call overload outcomes.
- Timeout request/reply is tested through runtime and simulator.
- Live substrate proves spawned-child cross-shard work and supervision restart.
- One named dogfood service exercises TCP ingress, timeout call, overload
  policy, restart, and replay/checker pressure.
- Claims say "try selected workloads" and still refuse broad production/Tokio
  replacement claims.

## Implementation Review — 486d126 "Add mandatory isolate call outcomes"

Session:

- B (review)

Reviewed commit `486d126` against Mercury build step 2 ("Timeout call").
Read the diff across `tina-runtime/src/call.rs`, `tina-runtime/src/lib.rs`,
`tina-runtime/src/trace.rs`, `tina-sim/src/lib.rs`, plus the new tests in
both crates' `consumer_api.rs`. Reran the full surface.

### What landed

- Public surface: `call(destination, message, timeout) -> IsolateCall<T, R>`
  followed by `.reply(translator)` returning `Effect<I>`. The translator
  receives `CallOutcome<R>` with four variants: `Replied(R)`, `Full`,
  `Closed`, `Timeout`.
- `RuntimeCall::isolate_call(destination, message, timeout, translator)`
  is the underlying constructor.
- `CallError` gains `TargetFull`, `TargetClosed`, `Timeout` for trace
  failure events.
- `CallKind::IsolateCall` is the new trace tag.
- Pending isolate-calls live in a per-runtime / per-sim
  `pending_isolate_calls` vec with deadline-ordered timeout harvest at
  the start of each step, before message delivery.
- Mailbox enqueue / dequeue grows a parallel "call_contexts" lane so a
  message arriving on the target can carry the requester's `call_id`
  back to the runtime when the target's handler returns `Reply(...)`.

### Same-shard-only scoping

Both runtime and simulator reject `target_shard != self.shard.id()`
at dispatch:

- one source-side `SendDispatchAttempted` event
- one source-side `SendRejected { reason: Closed }` event
- the call resolves immediately as `CallOutcome::Closed`
- one `CallFailed { kind: IsolateCall, reason: TargetClosed }` event

Honest and deterministic. Mercury build step 2 in this same commit is
amended to read "remaining: cross-shard call reply transport is not
claimed yet." Scoping is documented at the plan level.

### Test coverage against the asked focus

- **Reply delivery.** `WorkerRequest::ReplyNow` returns
  `reply(WorkerReply("pong"))`; caller observes
  `CallOutcome::Replied(WorkerReply("pong"))`; trace records one
  `CallCompleted { kind: IsolateCall }`. Runtime and sim.
- **Full.** Target capacity 0; outcome is `CallOutcome::Full`; trace
  records `CallFailed { reason: TargetFull }`. Runtime and sim.
- **Closed.** Target stopped before the call; outcome is
  `CallOutcome::Closed`; trace records
  `CallFailed { reason: TargetClosed }`. Runtime and sim.
- **Timeout.** Target handles `DoNotReply` with `noop()`; deadline
  elapses; outcome is `CallOutcome::Timeout`; trace records
  `CallFailed { reason: Timeout }`. Runtime and sim.
- **Requester-stopped.** Caller batches `[call(...), stop()]`; target
  replies later; trace records
  `CallCompletionRejected { kind: IsolateCall, reason: RequesterClosed }`;
  outcomes vec stays empty. Runtime and sim.
- **Replay determinism.** Sim test runs each scenario twice and
  asserts bytewise-equal `event_record()`; outcomes lists equal.
  Holds for reply, full, closed, timeout, and requester-stopped.

### Trace shape

- Source-side: `SendDispatchAttempted` → `SendAccepted` or
  `SendRejected` for the request delivery.
- Target-side: ordinary `MailboxAccepted` for the request.
- Target handler runs normally; its returning `Reply(reply)` is
  intercepted only when the message that triggered the handler
  carries a `MessageCallContext`. Then `EffectObserved { Reply }` is
  suppressed and `CallCompleted` (or `CallFailed` /
  `CallCompletionRejected`) is emitted on the requester instead.
- Causality: requester's `CallDispatchAttempted` is the cause for
  the eventual `CallCompleted` / `CallFailed` /
  `CallCompletionRejected`.

### What does not regress

- Ordinary `send(...)` calls go through `dispatch_local_send`, which
  delegates to `dispatch_local_send_with_context(send, None)`. No
  `call_context` is attached. Receiving handler's `Reply(...)` sees
  `call_context: None` and emits the existing
  `EffectObserved { Reply }`.
- Mailbox FIFO is preserved. The new
  `call_contexts: VecDeque<Option<MessageCallContext>>` on each
  runtime entry is updated only when `enqueue_entry_message`
  succeeds. `recv_entry_message` pops the message and then pops the
  matching context. Push and pop pair one-to-one with the mailbox.
- `try_send` (typed ingress) routes through `enqueue_entry_message`
  with `None` context.
- `harvest_remote_send` (cross-shard harvest) enqueues with `None`
  context. Cross-shard isolate calls are rejected at dispatch time
  and never reach a remote queue, so there is no missing context to
  worry about.
- Quiescence: `has_in_flight_calls` now includes
  `pending_isolate_calls`, so `run_until_quiescent` keeps stepping
  until pending calls resolve via reply or timeout.
- `cargo +nightly test -p tina-runtime -p tina-sim` is green across
  the full surface (46 inline runtime tests, 11 consumer-api
  integration tests, all multishard suites, all 016-020 suites).
  `make verify` exits 0.

### How this could still be broken while tests pass

- **Reply downcast type mismatch panics, no graceful path.** The
  translator captured in `IsolateCall::reply` does
  `*reply.downcast::<R>().unwrap_or_else(|_| panic!(...))`. If the
  target is mistyped or `R` is wrong on the caller side, the runtime
  panics inside the translator. The panic message names the expected
  type. Honest but loud. No `CallOutcome` variant for "reply was the
  wrong type." Probably out of scope for this slice.
- **Late reply after timeout fires.** Sequence:
  1. Caller dispatches with timeout 1ms.
  2. Step harvest runs `harvest_isolate_call_timeouts` — deadline
     elapsed — caller gets `Timeout` and the pending entry is removed.
  3. Same step or later, target handler runs and returns `Reply(...)`.
  4. `complete_isolate_call(call_id)` finds nothing and falls through
     to `EffectObserved { Reply }`.
  Reply value is silently dropped. Tests pass because the timeout
  test uses `DoNotReply` (target never replies). A test that exercises
  "late reply after timeout" would pin the trace shape (caller
  `CallFailed { Timeout }` plus target `EffectObserved { Reply }`
  with no shared call_id). Behavior itself is honest under
  explicit-step semantics.
- **Mailbox-full at requester on completion delivery.** Runtime's
  `deliver_isolate_call_outcome` enqueues the translator output via
  `enqueue_entry_message`. If full, it emits
  `CallCompletionRejected { reason: MailboxFull }`. The arm exists
  but is not directly proved by a test. Only the requester-closed arm
  is proven.
- **Translator runs even when the requester is stopped.** Path: target
  replies, `complete_isolate_call` finds the pending entry,
  `deliver_isolate_call_outcome`:
  1. Pushes `CallFailed` / no-failure event.
  2. Calls `translator(outcome)` to produce the requester message.
  3. Then checks if the requester is missing or stopped, and emits
     `CallCompletionRejected` if so, dropping the translated message.
  Translators with side effects would run even when the message goes
  nowhere. Pure translators are the convention; the runtime does not
  guard against effectful ones. Worth a doc note: translators must
  be pure.
- **Generation churn during a pending call.** If the target panics
  and is restarted between dispatch and reply, the request was
  enqueued on the old mailbox; the new mailbox is fresh. The pending
  entry in the caller still tracks the old call_id and times out at
  deadline. Honest, but no test directly exercises target restart
  during a pending call.
- **Cross-shard scoping surfaces as `Closed`, not a distinct variant.**
  A user reading `CallOutcome::Closed` does not learn from the variant
  alone that cross-shard is the actual reason. The trace shows
  `target_shard != self.shard.id()` implicitly through the
  `SendRejected { reason: Closed }` event fields, but the outcome
  variant collapses "target was stopped" and "target is on another
  shard" into the same shape. Documented at the plan level; worth a
  one-line doc-comment on `call(...)` and `RuntimeCall::isolate_call`.

### What old behavior is still at risk

- 016-020 suites pass without changes. The mailbox-FIFO and
  reply-event semantics are preserved by construction:
  - `Effect::Reply(R)` from a non-call-context message still produces
    `EffectObserved { Reply }`.
  - All existing `try_send` and `dispatch_local_send` callers route
    through `enqueue_entry_message(..., None)`.
- The `I::Reply: 'static` bound was added on `register*` and several
  impl blocks. Existing tests use ordinary types (`()`, `WorkerReply`,
  etc.) which are `'static`. No test surface migrated.
- Trace consumers that count `EffectObserved { Reply }` to estimate
  "all replies emitted" are now slightly off: replies that satisfy a
  call get `CallCompleted` instead. No test in the suite does this
  kind of counting today.

### Wall-clock vs virtual-time timeout testing

- Runtime test for timeout uses `Duration::from_millis(2)` plus
  `drive` which sleeps 1ms between steps. Real wall clock determines
  when the deadline fires. Reliable on a quiet machine, tight on a
  loaded CI runner. A `ManualClock` variant would be deterministic.
- Simulator test uses virtual time (`virtual_now + timeout`) and is
  fully deterministic. Replay equality holds.

### Recommendation

The implementation matches Mercury build-step-2 and the asked focus
areas. All five categories (reply, full / closed / timeout,
requester-stopped, trace events, replay determinism) are exercised
by named tests on both runtime and simulator. Same-shard scoping is
honestly enforced and deterministically traced. No regressions in
`send` / `reply` / mailbox ordering across 016-020 suites or
`make verify`.

Suggested non-blocking follow-ups:

1. Add a doc-line on `call(...)` and `RuntimeCall::isolate_call`
   stating same-shard only in this slice; cross-shard target
   surfaces as `CallOutcome::Closed`.
2. Add a `ManualClock` variant of the runtime timeout test to remove
   wall-clock sensitivity from CI.
3. Add a focused test for "late reply after timeout" to pin the
   trace shape.
4. Add a focused test for requester mailbox full at completion
   delivery on `IsolateCall`.
5. Add a doc note that translators must be pure; they may run even
   when the requester has stopped, before the `RequesterClosed`
   check.
6. Optional future: a distinct `CallOutcome` variant for cross-shard
   targets when cross-shard call reply transport lands.

None block this implementation closeout against Mercury build
step 2.

### Follow-up Resolution

After review, items 1-5 were addressed on the Mercury branch:

- same-shard-only and pure-translator notes were added to `call(...)` /
  `RuntimeCall::isolate_call(...)` docs
- runtime timeout now has a `ManualClock` proof
- late reply after timeout is pinned as ordinary `EffectObserved { Reply }`
- requester mailbox-full completion rejection is tested for `IsolateCall`
- simulator replay coverage was expanded for the new edge cases

Item 6 remains future work: a distinct cross-shard outcome belongs with real
cross-shard call reply transport.
