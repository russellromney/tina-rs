# Plan: 001 Mariner Trace Core

## Context

- The workspace currently ships `tina` and `tina-mailbox-spsc` only.
- `tina` already defines the shared trait surface: `Isolate`, closed `Effect`,
  `Mailbox`, `Context`, `Address`, `SendMessage`, `SpawnSpec`, and supervision
  policy types.
- There is no runtime crate yet.
- `SYSTEM.md` says the first runtime must introduce a deterministic runtime
  event trace with causal links, and it must not push runtime helpers into
  `tina`.
- The spec diff for this slice is intentionally narrow: add runtime trace types
  plus a tiny single-isolate `step_once` harness. Do not quietly slide into
  supervisor work, poll loops, socket code, or simulator work.

## References

- `.intent/SYSTEM.md`: current system intent and boundaries
- `.intent/phases/001-mariner-trace-core/spec-diff.md`: the approved intent
  change for this slice
- `ROADMAP.md`: Mariner proof goals and future phase boundaries
- `tina/src/lib.rs`: current shared trait and type surface
- `tina/tests/sputnik_api.rs`: downstream-style integration test style
- `tina/tests/supervision_policy.rs`: current test voice and assertion style

## Scope

1. Add `tina-runtime-current` to the workspace.
2. Create `tina-runtime-current/Cargo.toml` and `tina-runtime-current/src/lib.rs`.
3. In `tina-runtime-current`, add the first public runtime-trace surface:
   - `EventId`
   - `CauseId`
   - `EffectKind`
   - `RuntimeEvent`
   - `RuntimeEventKind`
4. Add a small stateful single-isolate runner in `tina-runtime-current` that:
   - keeps deterministic event-id allocation local to the runner
   - remembers whether the isolate has stopped
   - reads at most one message per `step_once`
   - runs one real handler through `Context`
   - records the matching runtime trace events for that step
5. Keep the first emitted trace vocabulary limited to events this slice can
   really produce. At minimum, cover:
   - mailbox accept
   - handler start
   - handler finish with effect kind
   - a post-handle event that records observed effects without executing them,
     except that `Effect::Stop` also records and applies the stopped state
6. Add integration tests in `tina-runtime-current/tests/` with a concrete
   deterministic mailbox implementation defined in the tests.
7. Prove:
   - one accepted message becomes exactly one handler invocation
   - repeated `step_once` calls preserve FIFO order
   - `Effect::Stop` prevents later delivery
   - two identical fresh runs produce the same event sequence
   - tests assert the exact causal chain for this slice:
     - mailbox accept is the root cause
     - handler start is caused by mailbox accept
     - handler finish is caused by handler start
     - the final post-handle event is caused by handler finish
   - two identical fresh runs produce the same causal links

## Traps

- Do not add runtime helpers or trace-specific types to `tina`. This slice
  belongs in `tina-runtime-current`.
- Do not couple the shared trace model to isolate-specific payload values.
  Trace shared runtime facts, not user message contents or reply data.
- Do not invent full future semantics for mailbox reject, spawn, restart, or
  simulator behavior if this slice cannot actually execute them yet.
- This slice only executes stop-state transitions. `Reply`, `Send`, `Spawn`,
  and `RestartChildren` are observed and traced, not executed.
- `Stop` is stateful. The runner must remember it across later `step_once`
  calls; it is not just a one-step event.
- Determinism means no timestamps, UUIDs, global counters, or unordered data
  structures in the trace model.
- Do not use the real SPSC mailbox crate for correctness tests unless mailbox
  behavior is the thing being tested. Use a tiny deterministic concrete mailbox
  in the test module instead.
- Do not add a Tokio poll loop, async task machinery, background threads, or
  socket code "for later" in this slice.
- Do not add a Tokio dependency unless the code in this slice truly needs it.

## Acceptance

- `tina-runtime-current` exists as a workspace member.
- The new crate exports public trace ID/types plus a one-step single-isolate
  runner.
- A single accepted message produces exactly one handler invocation and a stable
  causal trace.
- FIFO order survives repeated `step_once` calls over queued messages.
- After `Effect::Stop`, later `step_once` calls do not deliver more messages.
- Non-stop effects are observed and traced, not executed.
- The only runner state transition executed in this slice is stopping after
  `Effect::Stop`.
- Two identical fresh runs produce equal event sequences and equal causal links.
- `make verify` passes.

## Out of scope

- Supervisor mechanism
- Tokio poll loop
- TCP echo server
- Cross-isolate send routing
- Simulator work
- Mailbox semantics changes
- New mailbox crates
- Publish/docs/changelog work beyond what is needed for this slice

## Commands

- `source ~/.zshrc && make verify`
