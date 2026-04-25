# Review: 001 Mariner Trace Core

Artifacts reviewed:

- `.intent/phases/001-mariner-trace-core/spec-diff.md`
- `.intent/phases/001-mariner-trace-core/plan.md`
- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/trace_core.rs`

Verification reviewed:

- `make verify` passed on the current working tree

## Positive conformance review

Judgment: passes

This slice matches the spec diff closely.

- It adds the new `tina-runtime-current` crate.
- It adds the first runtime trace surface: `EventId`, `CauseId`,
  `EffectKind`, `RuntimeEvent`, and `RuntimeEventKind`.
- It adds a tiny single-isolate runner that:
  - reads one message from an injected mailbox
  - runs one real handler through `Context`
  - records the matching runtime events
- It proves the runtime behavior through trace assertions rather than only by
  prose.

The exact causal chain is now pinned and tested:

- mailbox accept is the root cause
- handler start is caused by mailbox accept
- handler finish is caused by handler start
- the final post-handle event is caused by handler finish

The slice also matches the intended execution boundary:

- `Effect::Stop` is the only effect that changes runner state
- `Reply`, `Send`, `Spawn`, and `RestartChildren` are observed and traced, not
  executed

## Negative conformance review

Judgment: passes with no blocking drift

I looked for accidental widening of scope and for changes outside the intended
 blast radius.

What did not drift:

- no runtime helper surface was added to `tina`
- no mailbox semantics changed
- no supervisor mechanism was introduced
- no Tokio poll loop was introduced
- no TCP or socket work was introduced
- no simulator work was introduced
- no cross-isolate routing was introduced

The main implementation lives in the new runtime crate, which matches
`SYSTEM.md` and the crate-boundary rules.

The tests use a small deterministic test mailbox instead of coupling runtime
 correctness to the SPSC mailbox crate. That is the right shape for this slice.

I do not see unrelated file churn or adjacent “cleanup” hidden inside the
 implementation.

## Adversarial review

Judgment: passes, with one follow-up risk to watch

I tried to break the main claims of the slice.

I looked for these failure modes:

- fake causal links such as all-`None` causes
- partial execution of non-stop effects hidden behind a vague trace event
- a runner that “works” once but does not actually preserve stop-state across
  later steps
- a determinism claim that only means “same event count,” not same sequence

The current slice defends against those well:

- the exact causal chain is asserted directly in the tests
- `Stop` is sticky across later `step_once` calls
- non-stop effects are asserted as traced-only
- identical runs compare whole trace equality, not just event count

The main follow-up risk is future slice creep. `EffectObserved` is a good event
for this narrow step, but later Mariner work could start slipping partial
dispatcher behavior into that same path. When the next slice adds real effect
execution, the trace model should get more specific rather than quietly
overloading the current event shape.

## Overall

This work conforms to the spec diff and the tightened plan.

It is a good first Mariner slice because it creates a real runtime trace model,
keeps the crate boundary clean, and proves a small but honest piece of runtime
 behavior without pretending the rest of Mariner already exists.
