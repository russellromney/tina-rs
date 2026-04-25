# Review: 002 Mariner Local Send Dispatch

Artifacts reviewed:

- `.intent/phases/002-mariner-local-send-dispatch/spec-diff.md`
- `.intent/phases/002-mariner-local-send-dispatch/plan.md`
- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/local_send_dispatch.rs`

Verification reviewed:

- `make verify` passed on the current working tree

## Positive conformance review

Judgment: passes

This slice matches the spec diff and the locked-in decisions in the plan.

What landed:

- A new `CurrentRuntime` exists alongside the slice-001 `SingleIsolateRunner`.
- Isolates can be registered through `register(...)`, which assigns deterministic
  identifiers starting at `1` and returns a typed `Address<I::Message>`.
- `step()` walks registered isolates in registration order and gives each
  isolate at most one delivery chance per round, by snapshotting one message
  per entry before any handler runs.
- Local same-shard `Effect::Send` dispatches into the target mailbox through
  the registry. The target handler runs on a later runtime step, not
  recursively inside the sender's handler.
- The trace is now Send-specific. `SendDispatchAttempted`, `SendAccepted`, and
  `SendRejected { reason }` replace the generic `EffectObserved` event for
  `Send`. Other effects keep `EffectObserved`.
- `Reply`, `Spawn`, and `RestartChildren` are observed and traced, not
  executed, exactly as in slice 001.
- `Effect::Stop` continues to be the only runner-level state transition the
  runtime executes itself, plus the new transitive close of the stopped
  isolate's mailbox.

The integration tests pin every claim from the spec diff:

- Registration order maps to stepping order and to deterministic isolate IDs.
- An accepted local send produces exactly one later handler invocation on the
  target, with the trace asserted whole including the full causal chain
  through `MailboxAccepted → HandlerStarted → HandlerFinished{Send} →
  SendDispatchAttempted → SendAccepted → ...later target step...`.
- A rejected local send is visible in the trace as `SendRejected { Full }`,
  and the rejected message is not silently buffered into the target.
- A send to an unknown isolate panics rather than producing a normal trace
  event.
- `Reply`, `Spawn`, and `RestartChildren` produce only generic
  `EffectObserved` events.
- Two identical fresh runs produce equal trace sequences and equal causal
  links.

## Negative conformance review

Judgment: passes with no blocking drift

I looked for accidental widening of scope and for changes outside the
intended blast radius.

What did not drift:

- `tina/src/lib.rs` is unchanged. No runtime helper surface, registry types,
  or scheduler abstractions leaked into the trait crate.
- No public `Scheduler` trait was added. The erased-runner machinery
  (`ErasedMailbox`, `ErasedHandler`, `ErasedEffect`, `MailboxAdapter`,
  `HandlerAdapter`, `RegisteredEntry`, `ErasedSend`) is fully private to
  `tina-runtime-current`.
- No public `IsolateRegistry` extensibility surface. The only public new type
  is `CurrentRuntime`, plus the new `SendRejectedReason` and the new
  `RuntimeEventKind` variants used by the trace.
- The slice does not use `EffectObserved` for `Send` anywhere. Send dispatch
  goes through the new `SendDispatchAttempted → SendAccepted | SendRejected`
  vocabulary exclusively.
- `Reply`, `Spawn`, and `RestartChildren` are not executed. The matching arms
  in `step()` only push `EffectObserved` and do not register, mint, or
  restart anything.
- Send dispatch does not recurse into the target handler. The implementation
  enqueues into the target mailbox during the sender's step and lets the
  next round deliver, which the integration tests verify directly.
- Stepping order does not depend on a hash map or any unordered collection.
  `entries: Vec<RegisteredEntry<S>>` is iterated in registration order.
- Tests do not synthesize target addresses by hand. They get every address
  back from `register(...)`. The one exception is the unknown-isolate panic
  test, which intentionally constructs an unregistered address to prove the
  panic path.
- A send to an unknown isolate is not silently dropped. The runtime panics,
  and the test exercises that with `catch_unwind`.
- The runtime correctness tests use a small `RefCell`/`Cell`-backed
  `TestMailbox` rather than the real SPSC mailbox, so runtime behavior is
  not coupled to mailbox concurrency semantics.
- No Tokio dependency was added. No async tasks, background threads,
  supervisor mechanism, sockets, or simulator hooks appeared.
- The slice-001 `SingleIsolateRunner` is preserved unchanged, so the
  trace-core tests remain valid.

The implementation lives entirely in the new runtime crate, which matches
`SYSTEM.md` and the crate-boundary rules.

I do not see unrelated file churn or adjacent "cleanup" hidden inside the
implementation.

## Adversarial review

Judgment: passes, with one follow-up risk to watch

I tried to break the main claims of the slice.

I looked for these failure modes:

- a step round that delivers to one isolate twice because of a same-round
  send-to-self
- a stepping order that drifts under message volume or mailbox emptiness
- a successful send that bypasses the trace
- a rejected send whose payload still ends up in the target mailbox
- non-determinism leaking through `Box<dyn Any>` allocations
- type-erasure unsoundness when message types differ between sender and target
- a way for `Reply`, `Spawn`, or `RestartChildren` to be silently executed

The current slice defends against those well:

- `step()` collects exactly one message per registered isolate before any
  handler runs. A send during this round pushes into the target's mailbox
  but cannot reach the target this round, because the target's slot in the
  round snapshot was already taken. Send-to-self has the same property.
- An empty mailbox produces `None` in the snapshot and is skipped without
  recording any trace event. A stopped isolate produces `None` regardless of
  mailbox contents.
- Every Send dispatch path emits `SendDispatchAttempted` first, and only
  then either `SendAccepted` or `SendRejected { reason }`. Both success and
  failure branches share the same attempt cause, so the trace cannot lose
  the dispatch event on the failure path.
- A rejected send returns the message inside `TrySendError::Full(_)` or
  `TrySendError::Closed(_)`, which the runtime translates to
  `SendRejectedReason::Full` or `SendRejectedReason::Closed`. The payload is
  dropped with the error rather than buffered. The rejection test asserts
  the trace event and asserts the audit isolate never observes the rejected
  payload.
- The trace contains no `Box<dyn Any>` pointers, no addresses, no allocator
  state, no timestamps, and no global counters. Determinism is asserted as
  whole-trace equality across two fresh runs, not just by event count.
- Type erasure is bounded by the public `register` signature, which forces
  `I::Send = SendMessage<Outbound>` and matches the `Outbound` to the typed
  `Address<M>` used by senders. A type mismatch at runtime is impossible
  through the public API; if a future change ever produced one, the
  downcast paths panic with explicit messages rather than silently
  delivering a wrong-type message.
- The Reply, Spawn, and RestartChildren arms in `step()` only push
  `EffectObserved` events. They do not call `register`, mint addresses,
  enqueue, or transition any state.

One follow-up risk to watch:

- Pre-stop messages already buffered in a soon-to-stop isolate's mailbox.
  When an isolate handles a message and returns `Effect::Stop`, the runtime
  marks the isolate stopped and closes its mailbox. Any messages that were
  enqueued earlier and have not yet been delivered remain in the mailbox
  and are never handled. Sends after the stop are correctly rejected with
  `SendRejectedReason::Closed`. This is consistent with what the slice
  claims, but it means that "send accepted in this round, target stops in
  the same round" can produce a message that is silently never handled.
  Slice 002 does not claim to address this, and the supervision mechanism
  in a later slice is the right place to decide whether to drain, abandon,
  or trace these messages. Worth pinning explicitly when supervision lands.

## Overall

This work conforms to the spec diff and to the locked-in decisions in the
plan.

It is a good Mariner slice because it adds real dispatcher behavior for
local same-shard `Send` without widening the runtime to multi-effect
execution, supervision, Tokio, or cross-shard routing. The trace model
becomes more specific exactly where dispatcher behavior is now real, and
stays generic where the runtime still only observes effects. Determinism is
proved against the actual emitted trace, not against prose in the roadmap.

The follow-up about pre-stop buffered messages is a question for the
supervision and stop-semantics work, not a defect in this slice.
